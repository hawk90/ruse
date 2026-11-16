//! `get_c_indent`: the indent of one line, with its comment, paren, brace and top-level parts.

use super::keys::CinKey;
use super::lex::{
    at, check_linecomment, cin_isbreak, cin_iscomment, cin_isdo, cin_islinecomment, is_white,
    last_byte, ncmp, skipwhite, starts, strend,
};
use super::options::getdigits;
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
    fn paren_indent(
        &mut self,
        theline: &[u8],
        paren: Pos,
        cur_curpos: Pos,
        cur_amount: &mut i32,
    ) -> i32 {
        let o = *self.o();
        let t0 = at(theline, 0);
        let mut our_paren_pos = paren;
        let mut amount: i32;
        if t0 == b')' && o.paren_prev != 0 {
            amount = self.indent_of(self.cur.lnum - 1);
        } else {
            amount = -1;
            let mut lnum = cur_curpos.lnum - 1;
            while lnum > our_paren_pos.lnum {
                let line = self.line(lnum);
                let mut l: &'a [u8] = &line[skipwhite(line, 0).min(line.len())..];
                if self.nocode(l, 0) {
                    lnum -= 1;
                    continue;
                }
                if self.cin_ispreproc_cont(&mut l, &mut lnum, &mut amount) {
                    lnum -= 1;
                    continue;
                }
                self.cur.lnum = lnum;
                if let Some(t) = self.ind_find_start_cors(None) {
                    lnum = t.lnum + 1;
                    lnum -= 1;
                    continue;
                }
                let mp = self.corr_ind_maxparen(cur_curpos);
                if let Some(t) = self.find_match_paren(mp) {
                    if t == our_paren_pos {
                        amount = self.indent_of(lnum);
                        if t0 == b')' {
                            if our_paren_pos.lnum != lnum && *cur_amount > amount {
                                *cur_amount = amount;
                            }
                            amount = -1;
                        }
                        break;
                    }
                }
                lnum -= 1;
            }
        }
        if amount == -1 {
            let mut ignore_paren_col = 0usize;
            let mut is_if_for_while = false;
            if o.if_for_while != 0 {
                let save = self.cur;
                let mut outermost;
                let mut t = Some(our_paren_pos);
                loop {
                    outermost = t.expect("set");
                    self.cur = outermost;
                    t = self.find_match_paren(o.maxparen);
                    if !t.is_some_and(|t| t.lnum == outermost.lnum) {
                        break;
                    }
                }
                self.cur = save;
                let line = self.line(outermost.lnum);
                let mut c = outermost.col;
                is_if_for_while = Self::cin_is_if_for_while_before_offset(line, &mut c);
            }
            let (a, lline, lstart) = self.skip_label(our_paren_pos.lnum);
            amount = a;
            let mut look = skipwhite(lline, lstart);
            let mut look_line = lline;
            if at(lline, look) == b'(' {
                let save_lnum = self.cur.lnum;
                self.cur.lnum = our_paren_pos.lnum;
                let line = self.cur_line();
                // `look` is relative to the line skip_label returned, which is this line.
                let look_col = look;
                self.cur.col = look_col + 1;
                if let Some(t) = self.findmatchlimit(b')', false, i64::from(o.maxparen)) {
                    if t.lnum == our_paren_pos.lnum && t.col < our_paren_pos.col {
                        ignore_paren_col = t.col + 1;
                    }
                }
                self.cur.lnum = save_lnum;
                look_line = line;
                look = look_col;
            }
            let look_c = at(look_line, look);
            if t0 == b')'
                || (o.unclosed == 0 && !is_if_for_while)
                || (o.unclosed_noignore == 0 && look_c == b'(' && ignore_paren_col == 0)
            {
                if t0 != b')' {
                    *cur_amount = i32::MAX;
                    let l = self.line(our_paren_pos.lnum);
                    if o.unclosed_wrapped != 0 && self.cin_ends_in(l, b"(") {
                        let mut n = 1;
                        for col in 0..our_paren_pos.col {
                            match at(l, col) {
                                b'(' | b'{' => n += 1,
                                b')' | b'}' if n > 1 => n -= 1,
                                _ => {}
                            }
                        }
                        our_paren_pos.col = 0;
                        amount += n * o.unclosed_wrapped;
                    } else if o.unclosed_whiteok != 0 {
                        our_paren_pos.col += 1;
                    } else {
                        let mut col = our_paren_pos.col + 1;
                        while is_white(at(l, col)) {
                            col += 1;
                        }
                        if at(l, col) != 0 {
                            our_paren_pos.col = col;
                        } else {
                            our_paren_pos.col += 1;
                        }
                    }
                }
                if our_paren_pos.col > 0 {
                    let col = i32::try_from(self.vcol(our_paren_pos.lnum, our_paren_pos.col))
                        .unwrap_or(i32::MAX);
                    if *cur_amount > col {
                        *cur_amount = col;
                    }
                }
            }
            if t0 == b')' && o.matching_paren != 0 {
                // Line up with the start of the matching paren line.
            } else if (o.unclosed == 0 && !is_if_for_while)
                || (o.unclosed_noignore == 0 && look_c == b'(' && ignore_paren_col == 0)
            {
                if *cur_amount != i32::MAX {
                    amount = *cur_amount;
                }
            } else {
                let l = self.line(our_paren_pos.lnum);
                let mut col = our_paren_pos.col;
                while our_paren_pos.col > ignore_paren_col {
                    our_paren_pos.col -= 1;
                    match at(l, our_paren_pos.col) {
                        b'(' => {
                            amount += o.unclosed2;
                            col = our_paren_pos.col;
                        }
                        b')' => {
                            amount -= o.unclosed2;
                            col = usize::MAX;
                        }
                        _ => {}
                    }
                }
                if col == usize::MAX {
                    amount += o.unclosed;
                } else {
                    self.cur = Pos {
                        lnum: our_paren_pos.lnum,
                        col,
                    };
                    if self.find_match_paren_after_brace(o.maxparen).is_some() {
                        amount += o.unclosed2;
                    } else if is_if_for_while {
                        amount += o.if_for_while;
                    } else {
                        amount += o.unclosed;
                    }
                }
                if *cur_amount < amount {
                    amount = *cur_amount;
                }
            }
        }
        amount
    }

    /// The brace branch of `get_c_indent`: inside the block opened by the `{` at `brace`. Returns
    /// `i32::MIN` for "the indent of the cursor line" (a matched `if`/`do`, nvim's `goto theend`).
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    fn brace_indent(
        &mut self,
        theline: &[u8],
        brace: Pos,
        cur_curpos: Pos,
        cur_amount: &mut i32,
        ind_continuation: &mut i32,
    ) -> i32 {
        const INITIAL: u8 = 0;
        const IF: u8 = 1;
        const DO: u8 = 2;
        const CASE: u8 = 3;
        const ANY: u8 = 4;
        const TERM: u8 = 5;
        const UNTERM: u8 = 6;
        const SCOPEDECL: u8 = 7;
        const NOBREAK: u8 = 8;
        const CPP_BASECLASS: u8 = 9;
        const ENUM_OR_INIT: u8 = 10;
        const JS_KEY: u8 = 11;
        const COMMA: u8 = 12;
        let o = *self.o();
        let t0 = at(theline, 0);
        let try_brace = brace;
        let mut ourscope = brace.lnum;
        let start = self.line(ourscope);
        let mut amount: i32;
        let start_brace; // 1: column 0, 2: at start, 3: at end
        let mut cache = (
            false,
            Pos {
                lnum: usize::MAX,
                col: 0,
            },
        );
        let mut lookfor_cpp_namespace = false;
        let mut cont_amount = 0i32;
        let mut added_to_amount = 0i32;
        let mut raw_string_start = 0usize;
        let look = skipwhite(start, 0);
        if at(start, look) == b'{' {
            amount = i32::try_from(self.vcol(brace.lnum, brace.col)).unwrap_or(i32::MAX);
            start_brace = if at(start, 0) == b'{' { 1 } else { 2 };
        } else {
            self.cur.lnum = ourscope;
            let mut lnum = ourscope;
            if self.find_last_paren(start, b'(', b')') {
                if let Some(t) = self.find_match_paren(o.maxparen) {
                    lnum = t.lnum;
                }
            }
            let cl = self.cur_line();
            if (o.js != 0 || o.keep_case_label != 0) && self.cin_iscase(cl, skipwhite(cl, 0), false)
            {
                amount = self.get_indent();
            } else if o.js != 0 {
                amount = self.indent_of(lnum);
            } else {
                amount = self.skip_label(lnum).0;
            }
            start_brace = 3;
        }
        let mut js_cur_has_key = o.js != 0 && self.cin_has_js_key(theline, 0);
        if t0 == b'}' {
            return amount + o.close_extra;
        }
        let mut lookfor = INITIAL;
        if self.cin_iselse(theline, 0) {
            lookfor = IF;
        } else if self.cin_iswhileofdo(theline, 0, cur_curpos.lnum) {
            lookfor = DO;
        }
        if lookfor != INITIAL {
            self.cur.lnum = cur_curpos.lnum;
            if self.find_match(lookfor == IF, ourscope) {
                return i32::MIN;
            }
        }
        if start_brace == 1 {
            amount = o.open_left_imag;
            lookfor_cpp_namespace = true;
        } else if start_brace == 2 && lookfor_cpp_namespace {
            lookfor_cpp_namespace = true;
        } else if start_brace == 3 {
            amount += o.open_imag;
            let cl = self.cur_line();
            let l = skipwhite(cl, 0);
            if self.cin_is_cpp_namespace(cl, l) {
                amount += o.cpp_namespace;
            } else if self.cin_is_cpp_extern_c(cl, l) {
                amount += o.cpp_extern_c;
            }
        } else {
            amount -= o.open_extra;
            if amount < 0 {
                amount = 0;
            }
        }
        let mut lookfor_break = false;
        if self.cin_iscase(theline, 0, false) {
            lookfor = CASE;
            amount += o.case;
        } else if self.cin_isscopedecl(theline, 0) {
            lookfor = SCOPEDECL;
            amount += o.scopedecl;
        } else {
            if o.case_break != 0 && cin_isbreak(theline, 0) {
                lookfor_break = true;
            }
            lookfor = INITIAL;
            amount += o.level;
        }
        let mut scope_amount = amount;
        let mut whilelevel = 0i32;
        self.cur = cur_curpos;
        'outer: loop {
            self.cur.lnum -= 1;
            self.cur.col = 0;
            if self.cur.lnum <= ourscope {
                if lookfor == ENUM_OR_INIT {
                    if self.cur.lnum == 0
                        || (self.cur.lnum as i64) < ourscope as i64 - i64::from(o.maxparen)
                    {
                        if cont_amount > 0 {
                            amount = cont_amount;
                        } else if o.js == 0 {
                            amount += *ind_continuation;
                        }
                        break;
                    }
                    if let Some(t) = self.ind_find_start_cors(None) {
                        self.cur.lnum = t.lnum + 1;
                        self.cur.col = 0;
                        continue;
                    }
                    let mut l = self.cur_line();
                    let mut ln = self.cur.lnum;
                    if self.cin_ispreproc_cont(&mut l, &mut ln, &mut amount) {
                        self.cur.lnum = ln;
                        continue;
                    }
                    self.cur.lnum = ln;
                    if self.nocode(l, 0) {
                        continue;
                    }
                    let terminated = self.isterminated(l, 0, false, true);
                    if start_brace != 1 || !self.cin_isfuncdecl(Some(l), self.cur.lnum, 0) {
                        if terminated == b',' {
                            break;
                        }
                        if terminated != b';' && self.cin_isinit() {
                            break;
                        }
                        if terminated == 0 || terminated == b'{' {
                            continue;
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
                            continue;
                        }
                    }
                    if cont_amount > 0 {
                        amount = cont_amount;
                    } else {
                        amount += *ind_continuation;
                    }
                } else if lookfor == UNTERM {
                    if cont_amount > 0 {
                        amount = cont_amount;
                    } else {
                        amount += *ind_continuation;
                    }
                } else {
                    if lookfor != TERM && lookfor != CPP_BASECLASS && lookfor != COMMA {
                        amount = scope_amount;
                        if t0 == b'{' {
                            amount += o.open_extra;
                            added_to_amount = o.open_extra;
                        }
                    }
                    if lookfor_cpp_namespace {
                        if self.cur.lnum == ourscope {
                            continue;
                        }
                        if self.cur.lnum == 0 || (self.cur.lnum as i64) < ourscope as i64 - 20 {
                            break;
                        }
                        if let Some(t) = self.ind_find_start_cors(None) {
                            self.cur.lnum = t.lnum + 1;
                            self.cur.col = 0;
                            continue;
                        }
                        let mut l = self.cur_line();
                        let mut ln = self.cur.lnum;
                        if self.cin_ispreproc_cont(&mut l, &mut ln, &mut amount) {
                            self.cur.lnum = ln;
                            continue;
                        }
                        self.cur.lnum = ln;
                        if self.cin_is_cpp_namespace(l, 0) {
                            amount += o.cpp_namespace - added_to_amount;
                            break;
                        } else if self.cin_is_cpp_extern_c(l, 0) {
                            amount += o.cpp_extern_c - added_to_amount;
                            break;
                        }
                        if self.nocode(l, 0) {
                            continue;
                        }
                    }
                }
                break;
            }
            if let Some(t) = self.ind_find_start_cors(Some(&mut raw_string_start)) {
                self.cur.lnum = t.lnum + 1;
                self.cur.col = 0;
                continue;
            }
            let l = self.cur_line();
            let iscase = self.cin_iscase(l, 0, false);
            if iscase || self.cin_isscopedecl(l, 0) {
                if lookfor == CPP_BASECLASS {
                    break;
                }
                if whilelevel > 0 {
                    continue;
                }
                if lookfor == UNTERM || lookfor == ENUM_OR_INIT {
                    if cont_amount > 0 {
                        amount = cont_amount;
                    } else {
                        amount += *ind_continuation;
                    }
                    break;
                }
                if (iscase && lookfor == CASE)
                    || (iscase && lookfor_break)
                    || (!iscase && lookfor == SCOPEDECL)
                {
                    match self.find_start_brace() {
                        None => {
                            amount = self.get_indent();
                            break;
                        }
                        Some(t) if t.lnum == ourscope => {
                            amount = self.get_indent();
                            break;
                        }
                        Some(_) => continue,
                    }
                }
                let n = self.get_indent_nolabel(self.cur.lnum);
                if lookfor == TERM {
                    if n != 0 {
                        amount = n;
                    }
                    if !lookfor_break {
                        break;
                    }
                }
                if n != 0 {
                    amount = n;
                    let cl = self.cur_line();
                    if let Some(p) = self.after_label(cl) {
                        if self.cin_is_cinword(cl, p) {
                            if t0 == b'{' {
                                amount += o.open_extra;
                            } else {
                                amount += o.level + o.no_brace;
                            }
                        }
                    }
                    break;
                }
                scope_amount = self.get_indent()
                    + if iscase {
                        o.case_code
                    } else {
                        o.scopedecl_code
                    };
                lookfor = if o.case_break != 0 { NOBREAK } else { ANY };
                continue;
            }
            if lookfor == CASE || lookfor == SCOPEDECL {
                if self.find_last_paren(l, b'{', b'}') {
                    if let Some(t) = self.find_start_brace() {
                        self.cur.lnum = t.lnum + 1;
                        self.cur.col = 0;
                    }
                }
                continue;
            }
            if o.js == 0 && self.cin_islabel() {
                let cl = self.cur_line();
                match self.after_label(cl) {
                    None => continue,
                    Some(p) if self.nocode(cl, p) => continue,
                    Some(_) => {}
                }
            }
            let mut l = self.cur_line();
            let mut ln = self.cur.lnum;
            let pre = self.cin_ispreproc_cont(&mut l, &mut ln, &mut amount);
            self.cur.lnum = ln;
            if pre || self.nocode(l, 0) {
                continue;
            }
            let mut n = false;
            if lookfor != TERM && o.cpp_baseclass > 0 {
                n = self.cin_is_cpp_baseclass(&mut cache);
                l = self.cur_line();
            }
            if n {
                if lookfor == UNTERM {
                    if cont_amount > 0 {
                        amount = cont_amount;
                    } else {
                        amount += *ind_continuation;
                    }
                } else if t0 == b'{' {
                    lookfor = UNTERM;
                    *ind_continuation = 0;
                    continue;
                } else {
                    amount = self.get_baseclass_amount(cache.1.col);
                }
                break;
            } else if lookfor == CPP_BASECLASS {
                if self.isterminated(l, 0, true, false) != 0 {
                    break;
                }
                continue;
            }
            let terminated = self.isterminated(l, 0, false, true);
            if js_cur_has_key {
                js_cur_has_key = false;
                if o.js != 0 && terminated == b',' {
                    lookfor = JS_KEY;
                }
            }
            if lookfor == JS_KEY && self.cin_has_js_key(l, 0) {
                amount = self.get_indent();
                break;
            }
            if lookfor == COMMA {
                if try_brace.lnum >= self.cur.lnum {
                    break;
                }
                if terminated == b',' {
                    break;
                }
                amount = self.get_indent();
                if self.cur.lnum - 1 == ourscope {
                    break;
                }
            }
            if terminated == 0 || (lookfor != UNTERM && terminated == b',') {
                if lookfor != ENUM_OR_INIT
                    && (at(l, skipwhite(l, 0)) == b'[' || last_byte(l) == b'[')
                {
                    amount += *ind_continuation;
                }
                let _ = self.find_last_paren(l, b'(', b')');
                let mp = self.corr_ind_maxparen(cur_curpos);
                let mut trypos = self.find_match_paren(mp);
                if trypos.is_some_and(|t| (t.lnum, t.col) < (try_brace.lnum, try_brace.col)) {
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
                        continue;
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
                    *cur_amount = self.get_indent();
                } else {
                    let (a, ll, ls) = self.skip_label(self.cur.lnum);
                    *cur_amount = a;
                    l = ll;
                    lstart = ls;
                }
                if terminated != b',' && lookfor != TERM && t0 == b'{' {
                    amount = *cur_amount;
                    if at(l, skipwhite(l, lstart)) != b'{' {
                        amount += o.open_extra;
                    }
                    if o.cpp_baseclass != 0 && o.js == 0 {
                        lookfor = CPP_BASECLASS;
                        continue;
                    }
                    break;
                }
                if self.cin_is_cinword(l, lstart) || self.cin_iselse(l, skipwhite(l, lstart)) {
                    if lookfor == UNTERM || lookfor == ENUM_OR_INIT {
                        if cont_amount > 0 {
                            amount = cont_amount;
                        } else {
                            amount += *ind_continuation;
                        }
                        break;
                    }
                    amount = *cur_amount;
                    if t0 == b'{' {
                        amount += o.open_extra;
                    }
                    if lookfor != TERM {
                        amount += o.level + o.no_brace;
                        break;
                    }
                    let cl = self.cur_line();
                    let ls = skipwhite(cl, 0);
                    if cin_isdo(cl, ls) {
                        if whilelevel == 0 {
                            break;
                        }
                        whilelevel -= 1;
                    }
                    if self.cin_iselse(cl, ls) && whilelevel == 0 {
                        if at(cl, ls) == b'}' {
                            self.cur.col = ls + 1;
                        }
                        match self.find_start_brace() {
                            None => break,
                            Some(t) => {
                                if !self.find_match(true, t.lnum) {
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    if lookfor == UNTERM {
                        if terminated == b',' {
                            amount += *ind_continuation;
                        }
                        break;
                    }
                    if lookfor == ENUM_OR_INIT {
                        if terminated == b',' {
                            if o.cpp_baseclass == 0 {
                                break;
                            }
                            lookfor = CPP_BASECLASS;
                            continue;
                        }
                        if amount > *cur_amount {
                            amount = *cur_amount;
                        }
                    } else {
                        let cl = self.cur_line();
                        amount = *cur_amount;
                        let n = strend(cl, 0);
                        if o.js != 0
                            && terminated == b','
                            && (at(cl, skipwhite(cl, 0)) == b']' || (n >= 2 && cl[n - 2] == b']'))
                        {
                            break;
                        }
                        if lookfor == INITIAL && terminated == b',' {
                            if o.js != 0 {
                                if cin_iscomment(cl, skipwhite(cl, 0)) {
                                    break;
                                }
                                lookfor = COMMA;
                                if let Some(t) = self.find_match_char(b'[', o.maxparen) {
                                    if t.lnum == self.cur.lnum - 1 {
                                        break;
                                    }
                                    ourscope = t.lnum;
                                }
                            } else {
                                lookfor = ENUM_OR_INIT;
                                cont_amount = self.cin_first_id_amount();
                            }
                        } else {
                            if lookfor == INITIAL && last_byte(cl) == b'\\' {
                                cont_amount = self.cin_get_equal_amount(self.cur.lnum);
                            }
                            if lookfor != TERM
                                && lookfor != JS_KEY
                                && lookfor != COMMA
                                && raw_string_start != self.cur.lnum
                            {
                                lookfor = UNTERM;
                            }
                        }
                    }
                }
            } else if self.cin_iswhileofdo_end(terminated) {
                if lookfor == UNTERM || lookfor == ENUM_OR_INIT {
                    if cont_amount > 0 {
                        amount = cont_amount;
                    } else {
                        amount += *ind_continuation;
                    }
                    break;
                }
                if whilelevel == 0 {
                    lookfor = TERM;
                    amount = self.get_indent();
                    if t0 == b'{' {
                        amount += o.open_extra;
                    }
                }
                whilelevel += 1;
            } else {
                if lookfor == NOBREAK {
                    let cl = self.cur_line();
                    if cin_isbreak(cl, skipwhite(cl, 0)) {
                        lookfor = ANY;
                        continue;
                    }
                }
                if whilelevel > 0 {
                    let cl = self.cur_line();
                    let s = self.skipcomment(cl, 0);
                    if cin_isdo(cl, s) {
                        amount = self.get_indent();
                        whilelevel -= 1;
                        continue;
                    }
                }
                if lookfor == UNTERM || lookfor == ENUM_OR_INIT {
                    if cont_amount > 0 {
                        amount = cont_amount;
                    } else {
                        amount += *ind_continuation;
                    }
                    break;
                }
                if lookfor == TERM {
                    if !lookfor_break && whilelevel == 0 {
                        break;
                    }
                } else {
                    // term_again:
                    loop {
                        let l = self.cur_line();
                        if self.find_last_paren(l, b'(', b')') {
                            if let Some(t) = self.find_match_paren(o.maxparen) {
                                self.cur = t;
                                let l = self.cur_line();
                                if self.cin_iscase(l, 0, false) || self.cin_isscopedecl(l, 0) {
                                    self.cur.lnum += 1;
                                    self.cur.col = 0;
                                    continue 'outer;
                                }
                            }
                        }
                        let cl = self.cur_line();
                        let iscase = o.keep_case_label != 0 && self.cin_iscase(cl, 0, false);
                        let (a, ll, ls) = self.skip_label(self.cur.lnum);
                        amount = a;
                        if t0 == b'{' {
                            amount += o.open_extra;
                        }
                        let li = skipwhite(ll, ls);
                        if at(ll, li) == b'{' {
                            amount -= o.open_extra;
                        }
                        lookfor = if iscase { ANY } else { TERM };
                        if lookfor == TERM
                            && at(ll, li) != b'}'
                            && self.cin_iselse(ll, li)
                            && whilelevel == 0
                        {
                            match self.find_start_brace() {
                                None => break 'outer,
                                Some(t) => {
                                    if !self.find_match(true, t.lnum) {
                                        break 'outer;
                                    }
                                }
                            }
                            continue 'outer;
                        }
                        let l = self.cur_line();
                        if self.find_last_paren(l, b'{', b'}') {
                            if let Some(t) = self.find_start_brace() {
                                self.cur = t;
                                let cl = self.cur_line();
                                let s = self.skipcomment(cl, 0);
                                if at(cl, s) == b'}' || !self.cin_iselse(cl, s) {
                                    continue; // term_again
                                }
                                self.cur.lnum += 1;
                                self.cur.col = 0;
                            }
                        }
                        break;
                    }
                }
            }
        }
        amount
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
