//! The syntactic predicates of `indent_c.c`: labels, `case`, scope declarations, function
//! declarations, C++ base classes, terminators and the `if`/`do` matching.

use super::lex::{
    at, char_len, cin_iscomment, cin_isdo, cin_isif, cin_ispreproc, cin_starts_with, is_idc,
    is_white, is_wordc, last_byte, parts, skip_string, skiptowhite, skipwhite, starts,
};
use super::{Ci, Pos};

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    pub(super) fn cin_has_js_key(&self, s: &[u8], i: usize) -> bool {
        let mut i = skipwhite(s, i);
        let mut quote = 0;
        if at(s, i) == b'\'' || at(s, i) == b'"' {
            quote = at(s, i);
            i += 1;
        }
        if !is_idc(at(s, i)) {
            return false;
        }
        while is_idc(at(s, i)) {
            i += 1;
        }
        if at(s, i) != 0 && at(s, i) == quote {
            i += 1;
        }
        let i = self.skipcomment(s, i);
        at(s, i) == b':' && at(s, i + 1) != b':'
    }

    /// `cin_islabel_skip`: whether `s[*i..]` is `label:`; `*i` moves past the `:` when it is.
    fn islabel_skip(&self, s: &[u8], i: &mut usize) -> bool {
        if !is_idc(at(s, *i)) {
            return false;
        }
        while is_idc(at(s, *i)) {
            *i += char_len(s, *i);
        }
        *i = self.skipcomment(s, *i);
        if at(s, *i) != b':' {
            return false;
        }
        *i += 1;
        at(s, *i) != b':'
    }

    /// `cin_islabel`: whether the cursor line is a jump label (`label:`).
    pub(super) fn cin_islabel(&mut self) -> bool {
        let line = self.cur_line();
        let mut s = self.skipcomment(line, 0);
        if self.cin_isdefault(line, s) || self.cin_isscopedecl(line, s) {
            return false;
        }
        if !self.islabel_skip(line, &mut s) {
            return false;
        }
        if self.ind_find_start_cors(None).is_some() {
            return false;
        }
        let save = self.cur;
        while self.cur.lnum > 1 {
            self.cur.lnum -= 1;
            self.cur.col = 0;
            if let Some(p) = self.ind_find_start_cors(None) {
                self.cur = p;
            }
            let line = self.cur_line();
            if cin_ispreproc(line) {
                continue;
            }
            let mut l = self.skipcomment(line, 0);
            if at(line, l) == 0 {
                continue;
            }
            self.cur = save;
            return self.isterminated(line, l, true, false) != 0
                || self.cin_isscopedecl(line, l)
                || self.cin_iscase(line, l, true)
                || (self.islabel_skip(line, &mut l) && self.nocode(line, l));
        }
        self.cur = save;
        true
    }

    fn skip_comment_and_string(&self, s: &[u8], mut p: usize) -> usize {
        loop {
            let r = p;
            p = self.skipcomment(s, p);
            if at(s, p) != 0 {
                p = skip_string(s, p);
            }
            if p == r {
                return p;
            }
        }
    }

    fn cin_is_compound_init(&self, s: &[u8], start: usize) -> bool {
        let mut p = start;
        let mut r: Option<usize> = None;
        while at(s, p) != 0 {
            if at(s, p) == b'=' {
                p = self.skipcomment(s, p + 1);
                r = Some(p);
            } else if starts(s, p, b"return")
                && !is_idc(at(s, p + 6))
                && (p == start || (p > start && !is_idc(s[p - 1])))
            {
                p = self.skipcomment(s, p + 6);
                r = Some(p);
            } else {
                p = self.skip_comment_and_string(s, p + 1);
            }
        }
        let Some(r) = r else {
            return false;
        };
        let mut p = r;
        if self.nocode(s, p) {
            return true;
        }
        if at(s, p) == b'&' {
            p = self.skipcomment(s, p + 1);
        }
        if at(s, p) == b'(' {
            let mut open: i32 = 1;
            loop {
                p = self.skip_comment_and_string(s, p + 1);
                if self.nocode(s, p) {
                    return true;
                }
                open += i32::from(at(s, p) == b'(') - i32::from(at(s, p) == b')');
                if open == 0 {
                    break;
                }
            }
            p = self.skipcomment(s, p + 1);
            if self.nocode(s, p) {
                return true;
            }
        }
        while at(s, p) == b'{' {
            p = self.skipcomment(s, p + 1);
        }
        self.nocode(s, p)
    }

    pub(super) fn cin_isinit(&self) -> bool {
        let line = self.cur_line();
        let mut s = self.skipcomment(line, 0);
        if cin_starts_with(line, s, b"typedef") {
            s = self.skipcomment(line, s + 7);
        }
        loop {
            let mut matched = false;
            for w in [&b"static"[..], b"public", b"protected", b"private"] {
                if cin_starts_with(line, s, w) {
                    s = self.skipcomment(line, s + w.len());
                    matched = true;
                    break;
                }
            }
            if !matched {
                break;
            }
        }
        if cin_starts_with(line, s, b"enum") {
            return true;
        }
        self.cin_is_compound_init(line, s)
    }

    /// `cin_iscase`: `case …:` or `default:`.
    pub(super) fn cin_iscase(&self, s: &[u8], i: usize, strict: bool) -> bool {
        let mut i = self.skipcomment(s, i);
        if cin_starts_with(s, i, b"case") {
            i += 4;
            while at(s, i) != 0 {
                i = self.skipcomment(s, i);
                if at(s, i) == 0 {
                    break;
                }
                if at(s, i) == b':' {
                    if at(s, i + 1) == b':' {
                        i += 1;
                    } else {
                        return true;
                    }
                }
                if at(s, i) == b'\'' && at(s, i + 1) != 0 && at(s, i + 2) == b'\'' {
                    i += 2;
                } else if at(s, i) == b'/' && (at(s, i + 1) == b'*' || at(s, i + 1) == b'/') {
                    return false;
                } else if at(s, i) == b'"' {
                    return !strict;
                }
                i += 1;
            }
            return false;
        }
        self.cin_isdefault(s, i)
    }

    fn cin_isdefault(&self, s: &[u8], i: usize) -> bool {
        if !starts(s, i, b"default") {
            return false;
        }
        let j = self.skipcomment(s, i + 7);
        at(s, j) == b':' && at(s, j + 1) != b':'
    }

    pub(super) fn cin_isscopedecl(&self, s: &[u8], i: usize) -> bool {
        let i = self.skipcomment(s, i);
        parts(self.e.cinscopedecls).any(|w| {
            if starts(s, i, w) {
                let k = self.skipcomment(s, i + w.len());
                at(s, k) == b':' && at(s, k + 1) != b':'
            } else {
                false
            }
        })
    }

    pub(super) fn cin_is_cpp_namespace(&self, s: &[u8], i: usize) -> bool {
        let mut i = self.skipcomment(s, i);
        while (starts(s, i, b"inline") || starts(s, i, b"export"))
            && (at(s, i + 6) == 0 || !is_wordc(at(s, i + 6)))
        {
            i = self.skipcomment(s, skipwhite(s, i + 6));
        }
        if starts(s, i, b"namespace") && (at(s, i + 9) == 0 || !is_wordc(at(s, i + 9))) {
            let mut p = self.skipcomment(s, skipwhite(s, i + 9));
            let mut has_name = false;
            let mut has_name_start = false;
            while at(s, p) != 0 {
                let c = at(s, p);
                if is_white(c) {
                    has_name = true;
                    p = self.skipcomment(s, skipwhite(s, p));
                } else if c == b'{' {
                    break;
                } else if is_wordc(c) {
                    has_name_start = true;
                    if has_name {
                        return false;
                    }
                    p += 1;
                } else if c == b':' && at(s, p + 1) == b':' && is_wordc(at(s, p + 2)) {
                    if !has_name_start || has_name {
                        return false;
                    }
                    p += 3;
                } else {
                    return false;
                }
            }
            return true;
        }
        false
    }

    /// `after_label`: the first code after the `:` of a label, or `None`.
    pub(super) fn after_label(&self, l: &[u8]) -> Option<usize> {
        let mut i = 0;
        while at(l, i) != 0 {
            if at(l, i) == b':' {
                if at(l, i + 1) == b':' {
                    i += 1;
                } else if !self.cin_iscase(l, i + 1, false) {
                    break;
                }
            } else if at(l, i) == b'\'' && at(l, i + 1) != 0 && at(l, i + 2) == b'\'' {
                i += 2;
            }
            i += 1;
        }
        if at(l, i) == 0 {
            return None;
        }
        let i = self.skipcomment(l, i + 1);
        if at(l, i) == 0 {
            return None;
        }
        Some(i)
    }

    pub(super) fn get_indent_nolabel(&self, lnum: usize) -> i32 {
        let l = self.line(lnum);
        match self.after_label(l) {
            Some(p) => i32::try_from(self.vcol(lnum, p)).unwrap_or(i32::MAX),
            None => 0,
        }
    }

    /// `skip_label`: the indent of line `lnum`, ignoring a case or jump label, and where its text
    /// (after the label) starts.
    pub(super) fn skip_label(&mut self, lnum: usize) -> (i32, &'a [u8], usize) {
        let save = self.cur;
        self.cur.lnum = lnum;
        let l = self.cur_line();
        let r = if self.cin_iscase(l, 0, false) || self.cin_isscopedecl(l, 0) || self.cin_islabel()
        {
            let amount = self.get_indent_nolabel(lnum);
            let l = self.cur_line();
            match self.after_label(l) {
                Some(p) => (amount, l, p),
                None => (amount, l, 0),
            }
        } else {
            (self.get_indent(), self.cur_line(), 0)
        };
        self.cur = save;
        r
    }

    pub(super) fn cin_first_id_amount(&self) -> i32 {
        let line = self.cur_line();
        let mut p = skipwhite(line, 0);
        let mut len = skiptowhite(line, p) - p;
        if len == 6 && starts(line, p, b"static") {
            p = skipwhite(line, p + 6);
            len = skiptowhite(line, p) - p;
        }
        if len == 6 && starts(line, p, b"struct") {
            p = skipwhite(line, p + 6);
        } else if len == 4 && starts(line, p, b"enum") {
            p = skipwhite(line, p + 4);
        } else if (len == 8 && starts(line, p, b"unsigned"))
            || (len == 6 && starts(line, p, b"signed"))
        {
            let s = skipwhite(line, p + len);
            if (starts(line, s, b"int") && is_white(at(line, s + 3)))
                || (starts(line, s, b"long") && is_white(at(line, s + 4)))
                || (starts(line, s, b"short") && is_white(at(line, s + 5)))
                || (starts(line, s, b"char") && is_white(at(line, s + 4)))
            {
                p = s;
            }
        }
        let mut len = 0;
        while is_idc(at(line, p + len)) {
            len += 1;
        }
        if len == 0 || !is_white(at(line, p + len)) || self.nocode(line, p) {
            return 0;
        }
        let p = skipwhite(line, p + len);
        i32::try_from(self.vcol(self.cur.lnum, p)).unwrap_or(i32::MAX)
    }

    pub(super) fn cin_get_equal_amount(&self, lnum: usize) -> i32 {
        if lnum > 1 && last_byte(self.line(lnum - 1)) == b'\\' {
            return -1;
        }
        let line = self.line(lnum);
        let mut s = 0;
        while at(line, s) != 0 && !b"=;{}\"'".contains(&at(line, s)) {
            if cin_iscomment(line, s) {
                s = self.skipcomment(line, s);
            } else {
                s += 1;
            }
        }
        if at(line, s) != b'=' {
            return 0;
        }
        let mut s = skipwhite(line, s + 1);
        if self.nocode(line, s) {
            return 0;
        }
        if at(line, s) == b'"' {
            s += 1;
        }
        i32::try_from(self.vcol(lnum, s)).unwrap_or(i32::MAX)
    }

    /// `cin_ispreproc_cont`: whether line `*lnum` (`*l`) is a preprocessor line or continues one; `*lnum`
    /// and `*l` move to its first line, and `*amount` takes its indent when it ends in a backslash.
    pub(super) fn cin_ispreproc_cont(
        &self,
        l: &mut &'a [u8],
        lnum: &mut usize,
        amount: &mut i32,
    ) -> bool {
        let mut line = *l;
        let mut n = *lnum;
        let mut retval = false;
        let mut candidate = *amount;
        if last_byte(line) == b'\\' {
            candidate = self.indent_of(n);
        }
        loop {
            if cin_ispreproc(line) {
                retval = true;
                *lnum = n;
                break;
            }
            if n <= 1 {
                break;
            }
            n -= 1;
            line = self.line(n);
            if last_byte(line) != b'\\' {
                break;
            }
        }
        if n != *lnum {
            *l = self.line(*lnum);
        }
        if retval {
            *amount = candidate;
        }
        retval
    }

    /// `cin_isterminated`: the char that ends the statement on `s[i..]` (`;`, `,`, `{`, `}`), or 0.
    pub(super) fn isterminated(&self, s: &[u8], i: usize, incl_open: bool, incl_comma: bool) -> u8 {
        let mut found_start = 0u8;
        let mut n_open = 0u32;
        let mut is_else = false;
        let mut i = self.skipcomment(s, i);
        if at(s, i) == b'{' || (at(s, i) == b'}' && !self.cin_iselse(s, i)) {
            found_start = at(s, i);
        }
        if found_start == 0 {
            is_else = self.cin_iselse(s, i);
        }
        while at(s, i) != 0 {
            i = skip_string(s, self.skipcomment(s, i));
            let c = at(s, i);
            if c == b'}' && n_open > 0 {
                n_open -= 1;
            }
            if (!is_else || n_open == 0)
                && (c == b';' || c == b'}' || (incl_comma && c == b','))
                && self.nocode(s, i + 1)
            {
                return c;
            } else if c == b'{' {
                if incl_open && self.nocode(s, i + 1) {
                    return c;
                }
                n_open += 1;
            }
            if at(s, i) != 0 {
                i += 1;
            }
        }
        found_start
    }

    pub(super) fn cin_iselse(&self, s: &[u8], mut i: usize) -> bool {
        if at(s, i) == b'}' {
            i = self.skipcomment(s, i + 1);
        }
        cin_starts_with(s, i, b"else")
    }

    /// `cin_isfuncdecl`: whether the line at `first_lnum` (or `sp`) looks like a function declaration:
    /// a `(` and a `)` ending the line (or a later one, after `,`), no `;` before.
    pub(super) fn cin_isfuncdecl(
        &mut self,
        sp: Option<&'a [u8]>,
        first_lnum: usize,
        min_lnum: usize,
    ) -> bool {
        let mut lnum = first_lnum;
        let save_lnum = self.cur.lnum;
        let mut s: &'a [u8] = sp.unwrap_or_else(|| self.line(lnum));
        let mut i = 0usize;
        self.cur.lnum = lnum;
        if self.find_last_paren(s, b'(', b')') {
            if let Some(t) = self.find_match_paren(self.o().maxparen) {
                lnum = t.lnum;
                if lnum < min_lnum {
                    self.cur.lnum = save_lnum;
                    return false;
                }
                s = self.line(lnum);
            }
        }
        self.cur.lnum = save_lnum;
        if cin_ispreproc(s) {
            return false;
        }
        while at(s, i) != 0 && !b"(;'\"".contains(&at(s, i)) {
            if cin_iscomment(s, i) {
                i = self.skipcomment(s, i);
            } else if at(s, i) == b':' {
                if at(s, i + 1) == b':' {
                    i += 2;
                } else {
                    return false;
                }
            } else {
                i += 1;
            }
        }
        if at(s, i) != b'(' {
            return false;
        }
        let mut just_started = true;
        while at(s, i) != 0 && !b";'\"".contains(&at(s, i)) {
            if at(s, i) == b')' && self.nocode(s, i + 1) {
                let prev = self.line(first_lnum.saturating_sub(1));
                return last_byte(prev) != b'\\';
            }
            if (at(s, i) == b',' && self.nocode(s, i + 1)) || at(s, i + 1) == 0 || self.nocode(s, i)
            {
                let comma = at(s, i) == b',';
                loop {
                    if lnum >= self.count() {
                        break;
                    }
                    lnum += 1;
                    s = self.line(lnum);
                    if !cin_ispreproc(s) {
                        break;
                    }
                }
                if lnum >= self.count() {
                    break;
                }
                i = skipwhite(s, 0);
                if !just_started && !comma && at(s, i) != b',' && at(s, i) != b')' {
                    break;
                }
                just_started = false;
            } else if cin_iscomment(s, i) {
                i = self.skipcomment(s, i);
            } else {
                i += 1;
                just_started = false;
            }
        }
        false
    }

    /// `cin_iswhileofdo`: a `while (cond);` that ends a `do`.
    pub(super) fn cin_iswhileofdo(&mut self, s: &[u8], i: usize, lnum: usize) -> bool {
        let mut p = self.skipcomment(s, i);
        if at(s, p) == b'}' {
            p = self.skipcomment(s, p + 1);
        }
        if !cin_starts_with(s, p, b"while") {
            return false;
        }
        let save = self.cur;
        self.cur = Pos { lnum, col: 0 };
        let line = self.cur_line();
        let mut q = 0;
        while at(line, q) != 0 && at(line, q) != b'w' {
            q += 1;
            self.cur.col += 1;
        }
        let mut retval = false;
        if let Some(t) = self.findmatchlimit(0, false, i64::from(self.o().maxparen)) {
            let l = self.line(t.lnum);
            if at(l, self.skipcomment(l, t.col + 1)) == b';' {
                retval = true;
            }
        }
        self.cur = save;
        retval
    }

    pub(super) fn cin_is_if_for_while_before_offset(line: &[u8], poffset: &mut usize) -> bool {
        let mut offset = *poffset as i64;
        if offset < 2 {
            return false;
        }
        offset -= 1;
        let get = |o: i64| if o < 0 { 0 } else { at(line, o as usize) };
        let st = |o: i64, w: &[u8]| o >= 0 && starts(line, o as usize, w);
        while offset > 2 && is_white(get(offset)) {
            offset -= 1;
        }
        offset -= 1;
        let mut found = st(offset, b"if");
        if !found && offset >= 1 {
            offset -= 1;
            found = st(offset, b"for");
            if !found && offset >= 2 {
                offset -= 2;
                found = st(offset, b"while");
            }
        }
        if !found {
            return false;
        }
        if offset == 0 || !is_idc(get(offset - 1)) {
            *poffset = offset as usize;
            return true;
        }
        false
    }

    /// `cin_iswhileofdo_end`: the cursor line ends a `do … while (cond);`; the cursor moves to the
    /// `while` line.
    pub(super) fn cin_iswhileofdo_end(&mut self, terminated: u8) -> bool {
        if terminated != b';' {
            return false;
        }
        let line = self.cur_line();
        let mut p = 0;
        while at(line, p) != 0 {
            p = self.skipcomment(line, p);
            if at(line, p) == b')' {
                let s = skipwhite(line, p + 1);
                if at(line, s) == b';' && self.nocode(line, s + 1) {
                    self.cur.col = p;
                    if let Some(t) = self.find_match_paren(self.o().maxparen) {
                        let l = self.line(t.lnum);
                        let mut s = self.skipcomment(l, 0);
                        if at(l, s) == b'}' {
                            s = self.skipcomment(l, s + 1);
                        }
                        if cin_starts_with(l, s, b"while") {
                            self.cur.lnum = t.lnum;
                            return true;
                        }
                    }
                }
            }
            if at(line, p) != 0 {
                p += 1;
            }
        }
        false
    }

    /// `cin_is_cpp_baseclass` (with its cache): the cursor line starts or continues a C++ base class
    /// list or constructor initializer; the cache holds where it lines up.
    pub(super) fn cin_is_cpp_baseclass(&mut self, cache: &mut (bool, Pos)) -> bool {
        let mut lnum = self.cur.lnum;
        let line = self.cur_line();
        if cache.1.lnum <= lnum {
            return cache.0;
        }
        cache.1.col = 0;
        let s = skipwhite(line, 0);
        if at(line, s) == b'#' {
            return false;
        }
        let s = self.skipcomment(line, s);
        if at(line, s) == 0 {
            return false;
        }
        let mut cpp_base_class = false;
        let mut lookfor_ctor_init = false;
        let mut class_or_struct = false;
        while lnum > 1 {
            let line = self.line(lnum - 1);
            let mut s = skipwhite(line, 0);
            if at(line, s) == b'#' || at(line, s) == 0 {
                break;
            }
            while at(line, s) != 0 {
                s = self.skipcomment(line, s);
                if at(line, s) == b'{'
                    || at(line, s) == b'}'
                    || (at(line, s) == b';' && self.nocode(line, s + 1))
                {
                    break;
                }
                if at(line, s) != 0 {
                    s += 1;
                }
            }
            if at(line, s) != 0 {
                break;
            }
            lnum -= 1;
        }
        cache.1.lnum = lnum;
        let mut line = self.line(lnum);
        let mut s = 0usize;
        loop {
            if at(line, s) == 0 {
                if lnum == self.cur.lnum {
                    break;
                }
                lnum += 1;
                line = self.line(lnum);
                s = 0;
            }
            if s == 0 {
                if self.cin_iscase(line, 0, false) {
                    break;
                }
                s = self.skipcomment(line, 0);
                if at(line, s) == 0 {
                    continue;
                }
            }
            let c = at(line, s);
            if c == b'"' || (c == b'R' && at(line, s + 1) == b'"') {
                s = skip_string(line, s) + 1;
            } else if c == b':' {
                if at(line, s + 1) == b':' {
                    lookfor_ctor_init = false;
                    s = self.skipcomment(line, s + 2);
                } else if lookfor_ctor_init || class_or_struct {
                    cpp_base_class = true;
                    lookfor_ctor_init = false;
                    class_or_struct = false;
                    cache.1.col = 0;
                    s = self.skipcomment(line, s + 1);
                } else {
                    s = self.skipcomment(line, s + 1);
                }
            } else if cin_starts_with(line, s, b"class") || cin_starts_with(line, s, b"struct") {
                class_or_struct = true;
                lookfor_ctor_init = false;
                s = self.skipcomment(line, s + if c == b'c' { 5 } else { 6 });
            } else {
                if c == b'{' || c == b'}' || c == b';' {
                    cpp_base_class = false;
                    lookfor_ctor_init = false;
                    class_or_struct = false;
                } else if c == b')' {
                    class_or_struct = false;
                    lookfor_ctor_init = true;
                } else if c == b'?' {
                    return false;
                } else if !is_idc(c) {
                    class_or_struct = false;
                    lookfor_ctor_init = false;
                } else if cache.1.col == 0 {
                    lookfor_ctor_init = false;
                    if cpp_base_class {
                        cache.1.col = s;
                    }
                }
                if lnum == self.cur.lnum && at(line, s) == b',' && self.nocode(line, s + 1) {
                    cache.1.col = 0;
                }
                s = self.skipcomment(line, s + 1);
            }
        }
        cache.0 = cpp_base_class;
        if cpp_base_class {
            cache.1.lnum = lnum;
        }
        cpp_base_class
    }

    pub(super) fn get_baseclass_amount(&mut self, col: usize) -> i32 {
        let mut amount;
        if col == 0 {
            amount = self.get_indent();
            let line = self.cur_line();
            if self.find_last_paren(line, b'(', b')') {
                if let Some(t) = self.find_match_paren(self.o().maxparen) {
                    amount = self.indent_of(t.lnum);
                }
            }
            if !self.cin_ends_in(self.cur_line(), b",") {
                amount += self.o().cpp_baseclass;
            }
        } else {
            self.cur.col = col;
            amount = i32::try_from(self.vcol(self.cur.lnum, col)).unwrap_or(i32::MAX);
        }
        amount.max(self.o().cpp_baseclass)
    }

    /// `cin_ends_in`: whether `s` ends in `find`, then only blanks and comments.
    pub(super) fn cin_ends_in(&self, s: &[u8], find: &[u8]) -> bool {
        let mut p = 0;
        while at(s, p) != 0 {
            p = self.skipcomment(s, p);
            if starts(s, p, find) {
                let r = skipwhite(s, p + find.len());
                if self.nocode(s, r) {
                    return true;
                }
            }
            if at(s, p) != 0 {
                p += 1;
            }
        }
        false
    }

    pub(super) fn cin_is_cpp_extern_c(&self, s: &[u8], i: usize) -> bool {
        let i = self.skipcomment(s, i);
        if starts(s, i, b"extern") && (at(s, i + 6) == 0 || !is_wordc(at(s, i + 6))) {
            let mut p = self.skipcomment(s, skipwhite(s, i + 6));
            let mut has = false;
            while at(s, p) != 0 {
                if is_white(at(s, p)) {
                    p = self.skipcomment(s, skipwhite(s, p));
                } else if at(s, p) == b'{' {
                    break;
                } else if starts(s, p, b"\"C\"") {
                    if has {
                        return false;
                    }
                    has = true;
                    p += 3;
                } else if starts(s, p, b"\"C++\"") {
                    if has {
                        return false;
                    }
                    has = true;
                    p += 5;
                } else {
                    return false;
                }
            }
            return has;
        }
        false
    }

    /// `cin_skip2pos`: past strings, chars and comments up to `pos`; the column reached.
    pub(super) fn cin_skip2pos(&self, pos: Pos) -> usize {
        let line = self.line(pos.lnum);
        let mut p = 0;
        while at(line, p) != 0 && p < pos.col {
            if cin_iscomment(line, p) {
                p = self.skipcomment(line, p);
            } else {
                let n = skip_string(line, p);
                p = if n == p { p + 1 } else { n };
            }
        }
        p
    }

    pub(super) fn cin_is_cinword(&self, s: &[u8], i: usize) -> bool {
        let i = skipwhite(s, i);
        parts(self.e.cinwords).any(|w| {
            starts(s, i, w) && (!is_wordc(at(s, i + w.len())) || !is_wordc(w[w.len() - 1]))
        })
    }

    /// `find_match`: from the cursor, the `if` an `else` belongs to (`LOOKFOR_IF`) or the `do` of a
    /// `while` (`LOOKFOR_DO`) in scope `ourscope`; the cursor rests on it.
    pub(super) fn find_match(&mut self, lookfor_if: bool, ourscope: usize) -> bool {
        let (mut elselevel, mut whilelevel) = if lookfor_if { (1, 0) } else { (0, 1) };
        self.cur.col = 0;
        while self.cur.lnum > ourscope + 1 {
            self.cur.lnum -= 1;
            self.cur.col = 0;
            let line = self.cur_line();
            let look = self.skipcomment(line, 0);
            if !self.cin_iselse(line, look)
                && !cin_isif(line, look)
                && !cin_isdo(line, look)
                && !self.cin_iswhileofdo(line, look, self.cur.lnum)
            {
                continue;
            }
            let Some(theirscope) = self.find_start_brace() else {
                break;
            };
            if theirscope.lnum < ourscope {
                break;
            }
            if theirscope.lnum > ourscope {
                continue;
            }
            let line = self.cur_line();
            let look = self.skipcomment(line, 0);
            if !(lookfor_if && whilelevel != 0) {
                if self.cin_iselse(line, look) {
                    let m = self.skipcomment(line, look + 4);
                    if !cin_isif(line, m) {
                        elselevel += 1;
                    }
                    continue;
                }
                if cin_isif(line, look) {
                    elselevel -= 1;
                    if elselevel == 0 && lookfor_if {
                        whilelevel = 0;
                    }
                }
            }
            if self.cin_iswhileofdo(line, look, self.cur.lnum) {
                whilelevel += 1;
                continue;
            }
            if cin_isdo(line, look) {
                whilelevel -= 1;
            }
            if elselevel <= 0 && whilelevel <= 0 {
                return true;
            }
        }
        false
    }
}
