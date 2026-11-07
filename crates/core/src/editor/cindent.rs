//! C indent (`'cindent'`, #1163): a port of nvim v0.12.6's `get_c_indent` and its helpers
//! (`indent_c.c`), with the parts of `findmatchlimit` (`search.c`) they run: the backward search for a
//! `/*` comment start or a C++ raw string start, and the bracket search that skips `"strings"`,
//! `'c'` characters and escaped brackets. `'cinoptions'` is parsed into [`CinOptions`] as `parse_cino`
//! does, and [`Ci::in_cinkeys`] reads `'cinkeys'` as `in_cinkeys` does.
//!
//! The port keeps nvim's structure (a cursor that the helpers move, 1-based line numbers, a NUL past the
//! end of each line) so each rule can be checked against the C source line by line. nvim's default
//! `'cpoptions'` is assumed (no `%`, no `M`), and `'lisp'` off: under `'lisp'` the Lisp indent wins.
//!
//! A line's indent depends on the lines above it and, for a function type spec, on the line below, so
//! the callers pass the whole buffer.

use super::range::cluster_cells;

/// `'cinkeys'` at its nvim default.
pub const DEFAULT_CINKEYS: &str = "0{,0},0),0],:,0#,!^F,o,O,e";
/// `'cinscopedecls'` at its nvim default.
pub const DEFAULT_CINSCOPEDECLS: &str = "public,protected,private";

/// `'cinoptions'` parsed (nvim's `b_ind_*` values, in columns), for a given `'shiftwidth'`.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CinOptions {
    pub level: i32,
    pub open_imag: i32,
    pub no_brace: i32,
    pub first_open: i32,
    pub open_extra: i32,
    pub close_extra: i32,
    pub open_left_imag: i32,
    pub jump_label: i32,
    pub case: i32,
    pub case_code: i32,
    pub case_break: i32,
    pub scopedecl: i32,
    pub scopedecl_code: i32,
    pub param: i32,
    pub func_type: i32,
    pub cpp_baseclass: i32,
    pub continuation: i32,
    pub unclosed: i32,
    pub unclosed2: i32,
    pub unclosed_noignore: i32,
    pub unclosed_wrapped: i32,
    pub unclosed_whiteok: i32,
    pub matching_paren: i32,
    pub paren_prev: i32,
    pub comment: i32,
    pub in_comment: i32,
    pub in_comment2: i32,
    pub maxparen: i32,
    pub maxcomment: i32,
    pub java: i32,
    pub js: i32,
    pub keep_case_label: i32,
    pub cpp_namespace: i32,
    pub if_for_while: i32,
    pub hash_comment: i32,
    pub cpp_extern_c: i32,
    pub pragma: i32,
}

impl CinOptions {
    /// `parse_cino`: the defaults for shiftwidth `sw`, then each `{flag}{N}` / `{flag}{N}s` /
    /// `{flag}-{N}` / `{flag}{N}.{M}s` item of `cino` (comma-separated, unknown flags ignored).
    #[must_use]
    pub fn parse(cino: &str, sw: usize) -> CinOptions {
        let sw = i32::try_from(sw).unwrap_or(i32::MAX);
        let mut o = CinOptions {
            level: sw,
            open_imag: 0,
            no_brace: 0,
            first_open: 0,
            open_extra: 0,
            close_extra: 0,
            open_left_imag: 0,
            jump_label: -1,
            case: sw,
            case_code: sw,
            case_break: 0,
            scopedecl: sw,
            scopedecl_code: sw,
            param: sw,
            func_type: sw,
            cpp_baseclass: sw,
            continuation: sw,
            unclosed: sw.saturating_mul(2),
            unclosed2: sw,
            unclosed_noignore: 0,
            unclosed_wrapped: 0,
            unclosed_whiteok: 0,
            matching_paren: 0,
            paren_prev: 0,
            comment: 0,
            in_comment: 3,
            in_comment2: 0,
            maxparen: 20,
            maxcomment: 70,
            java: 0,
            js: 0,
            keep_case_label: 0,
            cpp_namespace: 0,
            if_for_while: 0,
            hash_comment: 0,
            cpp_extern_c: 0,
            pragma: 0,
        };
        let s = cino.as_bytes();
        let sw = i64::from(sw);
        let mut p = 0usize;
        let mut fraction: i64 = 0;
        while p < s.len() {
            let l = p;
            p += 1;
            if at(s, p) == b'-' {
                p += 1;
            }
            let digits_start = p;
            let mut n = getdigits(s, &mut p);
            let mut divider: i64 = 0;
            if at(s, p) == b'.' {
                p += 1;
                fraction = atoi(s, p);
                while at(s, p).is_ascii_digit() {
                    p += 1;
                    divider = if divider != 0 { divider * 10 } else { 10 };
                }
            }
            if at(s, p) == b's' {
                if p == digits_start {
                    n = sw;
                } else {
                    n = n.saturating_mul(sw);
                    if divider != 0 {
                        n = n.saturating_add((sw * fraction + divider / 2) / divider);
                    }
                }
                p += 1;
            }
            if at(s, l + 1) == b'-' {
                n = -n;
            }
            let n = i32::try_from(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
                .unwrap_or_default();
            match s[l] {
                b'>' => o.level = n,
                b'e' => o.open_imag = n,
                b'n' => o.no_brace = n,
                b'f' => o.first_open = n,
                b'{' => o.open_extra = n,
                b'}' => o.close_extra = n,
                b'^' => o.open_left_imag = n,
                b'L' => o.jump_label = n,
                b':' => o.case = n,
                b'=' => o.case_code = n,
                b'b' => o.case_break = n,
                b'p' => o.param = n,
                b't' => o.func_type = n,
                b'/' => o.comment = n,
                b'c' => o.in_comment = n,
                b'C' => o.in_comment2 = n,
                b'i' => o.cpp_baseclass = n,
                b'+' => o.continuation = n,
                b'(' => o.unclosed = n,
                b'u' => o.unclosed2 = n,
                b'U' => o.unclosed_noignore = n,
                b'W' => o.unclosed_wrapped = n,
                b'w' => o.unclosed_whiteok = n,
                b'm' => o.matching_paren = n,
                b'M' => o.paren_prev = n,
                b')' => o.maxparen = n,
                b'*' => o.maxcomment = n,
                b'g' => o.scopedecl = n,
                b'h' => o.scopedecl_code = n,
                b'j' => o.java = n,
                b'J' => o.js = n,
                b'l' => o.keep_case_label = n,
                b'#' => o.hash_comment = n,
                b'N' => o.cpp_namespace = n,
                b'k' => o.if_for_while = n,
                b'E' => o.cpp_extern_c = n,
                b'P' => o.pragma = n,
                _ => {}
            }
            if at(s, p) == b',' {
                p += 1;
            }
        }
        o
    }
}

/// nvim's `getdigits`: a decimal number (an optional sign, then digits) at `*p`, or 0 when there is
/// none; `*p` moves past it.
fn getdigits(s: &[u8], p: &mut usize) -> i64 {
    let start = *p;
    let mut i = *p;
    let neg = match at(s, i) {
        b'-' => {
            i += 1;
            true
        }
        b'+' => {
            i += 1;
            false
        }
        _ => false,
    };
    let ds = i;
    let mut n: i64 = 0;
    while at(s, i).is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add(i64::from(at(s, i) - b'0'));
        i += 1;
    }
    if i == ds {
        *p = start;
        return 0;
    }
    *p = i;
    if neg {
        -n
    } else {
        n
    }
}

/// C's `atoi` at `p`: the leading digits as a number, 0 when there are none.
fn atoi(s: &[u8], p: usize) -> i64 {
    let mut q = p;
    getdigits(s, &mut q)
}

/// What the cindent engine reads besides the buffer: the buffer's indent options.
#[derive(Clone, Copy, Debug)]
pub struct CEnv<'a> {
    /// `'tabstop'` and `'vartabstop'`.
    pub ts: crate::cells::Tabs,
    /// `'cinoptions'`, parsed.
    pub cino: CinOptions,
    /// `'cinwords'`.
    pub cinwords: &'a str,
    /// `'cinscopedecls'`.
    pub cinscopedecls: &'a str,
    /// `'cinkeys'`.
    pub cinkeys: &'a str,
    /// `'comments'`.
    pub comments: &'a str,
}

/// A position: 1-based line number, byte column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
struct Pos {
    lnum: usize,
    col: usize,
}

/// The byte at `i`, or NUL past the end (a NUL byte in the line also ends it, as in nvim).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `strlen` from `i`: where the line ends (its first NUL byte, or its length).
fn strend(s: &[u8], i: usize) -> usize {
    match s.get(i..) {
        Some(t) => i + t.iter().position(|&c| c == 0).unwrap_or(t.len()),
        None => i,
    }
}

/// The line's last byte (`line[strlen(line) - 1]`), NUL when it is empty.
fn last_byte(s: &[u8]) -> u8 {
    let n = strend(s, 0);
    if n == 0 {
        0
    } else {
        s[n - 1]
    }
}

fn is_white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn skipwhite(s: &[u8], mut i: usize) -> usize {
    while is_white(at(s, i)) {
        i += 1;
    }
    i
}

fn skiptowhite(s: &[u8], mut i: usize) -> usize {
    while at(s, i) != 0 && !is_white(at(s, i)) {
        i += 1;
    }
    i
}

/// `vim_isIDc` with nvim's default `'isident'` (`@,48-57,_,192-255`), byte-wise.
fn is_idc(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 192
}

/// `vim_iswordc` with nvim's default `'iskeyword'` (`@,48-57,_,192-255`), byte-wise.
fn is_wordc(c: u8) -> bool {
    is_idc(c)
}

/// `strncmp(s + i, w, w.len()) == 0`.
fn starts(s: &[u8], i: usize, w: &[u8]) -> bool {
    s.get(i..).is_some_and(|t| t.starts_with(w))
}

/// The byte length of the UTF-8 char at `i` (1 for an invalid byte or past the end).
fn char_len(s: &[u8], i: usize) -> usize {
    let c = at(s, i);
    let n = match c {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    };
    if n > 1 && (1..n).all(|k| at(s, i + k) & 0xC0 == 0x80) {
        n
    } else {
        1
    }
}

/// `utf_head_off`: how far `i` is past the start of the char it is in.
fn head_off(s: &[u8], i: usize) -> usize {
    let mut k = 0;
    while k < 3 && i > k && at(s, i - k) & 0xC0 == 0x80 {
        k += 1;
    }
    if k > 0 && char_len(s, i - k) > k {
        k
    } else {
        0
    }
}

/// `skip_string`: past a `"string"`, `'c'` character or C++ raw string at `p` (they may run on:
/// `"a""b"`), else `p`; a string that runs to the end of the line leaves `p` on its last byte.
fn skip_string(s: &[u8], mut p: usize) -> usize {
    loop {
        if at(s, p) == b'\'' {
            if at(s, p + 1) == 0 {
                break;
            }
            let mut i = 2;
            if at(s, p + 1) == b'\\' && at(s, p + 2) != 0 {
                i += 1;
                while at(s, p + i - 1).is_ascii_digit() {
                    i += 1;
                }
            }
            if at(s, p + i - 1) != 0 && at(s, p + i) == b'\'' {
                p += i + 1;
                continue;
            }
        } else if at(s, p) == b'"' {
            p += 1;
            while at(s, p) != 0 {
                if at(s, p) == b'\\' && at(s, p + 1) != 0 {
                    p += 1;
                } else if at(s, p) == b'"' {
                    break;
                }
                p += 1;
            }
            if at(s, p) == b'"' {
                p += 1;
                continue;
            }
        } else if at(s, p) == b'R' && at(s, p + 1) == b'"' {
            let delim = p + 2;
            let end = strend(s, delim);
            if let Some(off) = s[delim.min(end)..end].iter().position(|&c| c == b'(') {
                let d = &s[delim..delim + off];
                p += 3;
                while at(s, p) != 0 {
                    if at(s, p) == b')' && starts(s, p + 1, d) && at(s, p + d.len() + 1) == b'"' {
                        p += d.len() + 1;
                        break;
                    }
                    p += 1;
                }
                if at(s, p) == b'"' {
                    p += 1;
                    continue;
                }
            }
        }
        break;
    }
    if at(s, p) == 0 {
        p = p.saturating_sub(1);
    }
    p
}

/// `is_pos_in_string`: whether `line[col]` is inside a C string.
fn is_pos_in_string(line: &[u8], col: usize) -> bool {
    let mut p = 0;
    while at(line, p) != 0 && p < col {
        p = skip_string(line, p) + 1;
    }
    p > col
}

/// `check_linecomment` (not `'lisp'`): the column of the line's `//` comment, if any.
pub(crate) fn check_linecomment(line: &[u8]) -> Option<usize> {
    let end = strend(line, 0);
    let mut p = 0;
    while let Some(off) = line[p.min(end)..end].iter().position(|&c| c == b'/') {
        let i = p + off;
        if at(line, i + 1) == b'/'
            && (i == 0 || line[i - 1] != b'*' || at(line, i + 2) != b'*')
            && !is_pos_in_string(line, i)
        {
            return Some(i);
        }
        p = i + 1;
    }
    None
}

/// `strncmp(a, b, n) == 0` over NUL-terminated strings.
fn ncmp(a: &[u8], b: &[u8], n: usize) -> bool {
    for i in 0..n {
        let (x, y) = (at(a, i), at(b, i));
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

fn cin_iscomment(s: &[u8], p: usize) -> bool {
    at(s, p) == b'/' && (at(s, p + 1) == b'*' || at(s, p + 1) == b'/')
}

fn cin_islinecomment(s: &[u8], p: usize) -> bool {
    at(s, p) == b'/' && at(s, p + 1) == b'/'
}

fn cin_ispreproc(s: &[u8]) -> bool {
    at(s, skipwhite(s, 0)) == b'#'
}

fn cin_isif(s: &[u8], p: usize) -> bool {
    starts(s, p, b"if") && !is_idc(at(s, p + 2))
}

fn cin_isdo(s: &[u8], p: usize) -> bool {
    starts(s, p, b"do") && !is_idc(at(s, p + 2))
}

fn cin_isbreak(s: &[u8], p: usize) -> bool {
    starts(s, p, b"break") && !is_idc(at(s, p + 5))
}

fn cin_starts_with(s: &[u8], p: usize, word: &[u8]) -> bool {
    starts(s, p, word) && !is_idc(at(s, p + word.len()))
}

/// The comma-separated parts of an option value.
fn parts(opt: &str) -> impl Iterator<Item = &[u8]> {
    opt.as_bytes()
        .split(|&c| c == b',')
        .filter(|p| !p.is_empty())
}

/// The C indent engine over the lines of a buffer, with nvim's moving cursor.
pub(crate) struct Ci<'a, L: AsRef<[u8]>> {
    lines: &'a [L],
    e: &'a CEnv<'a>,
    cur: Pos,
    /// nvim's `State & MODE_INSERT` while indenting.
    insert: bool,
    /// Per line, [`line_marks`]: which comment or raw-string marks it holds, so the backward searches
    /// for them step over a line without any in one go. `None`: worked out per line.
    marks: Option<&'a [u8]>,
}

/// A line holds `/*` or `*/`.
const MARK_COMMENT: u8 = 1;
/// A line holds `R"`.
const MARK_RAW: u8 = 2;

/// The [`MARK_COMMENT`] / [`MARK_RAW`] bits of `line`.
fn line_marks(line: &[u8]) -> u8 {
    let mut m = 0;
    for w in line.windows(2) {
        match w {
            b"/*" | b"*/" => m |= MARK_COMMENT,
            b"R\"" => m |= MARK_RAW,
            _ => {}
        }
    }
    m
}

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    pub(crate) fn new(lines: &'a [L], e: &'a CEnv<'a>) -> Self {
        Ci {
            lines,
            e,
            cur: Pos { lnum: 1, col: 0 },
            insert: false,
            marks: None,
        }
    }

    /// Whether line `lnum` holds the mark `bit`.
    fn has_mark(&self, lnum: usize, bit: u8) -> bool {
        match self.marks.and_then(|m| m.get(lnum.max(1) - 1)) {
            Some(&m) => m & bit != 0,
            None => line_marks(self.line(lnum)) & bit != 0,
        }
    }

    fn o(&self) -> &CinOptions {
        &self.e.cino
    }

    fn count(&self) -> usize {
        self.lines.len().max(1)
    }

    /// `ml_get`: line `lnum` (line 0 reads as line 1, past the end as `???`).
    fn line(&self, lnum: usize) -> &'a [u8] {
        if lnum > self.lines.len() {
            return b"???";
        }
        match self.lines.get(lnum.max(1) - 1) {
            Some(l) => l.as_ref(),
            None => b"",
        }
    }

    fn cur_line(&self) -> &'a [u8] {
        self.line(self.cur.lnum)
    }

    /// The virtual column where the char at byte `col` of line `lnum` starts (`getvcol`).
    fn vcol(&self, lnum: usize, col: usize) -> usize {
        let l = self.line(lnum);
        let end = strend(l, 0);
        let mut v = 0;
        let mut i = 0;
        while i < col.min(end) {
            let (next, w) = cluster_cells(l, i, end, v, self.e.ts);
            v += w;
            i = next;
        }
        v
    }

    /// `get_indent_lnum`: the indent of line `lnum` in columns.
    fn indent_of(&self, lnum: usize) -> i32 {
        let l = self.line(lnum);
        let ts = self.e.ts;
        let mut v = 0usize;
        for &c in l {
            match c {
                b' ' => v += 1,
                b'\t' => v += ts.padding(v),
                _ => break,
            }
        }
        i32::try_from(v).unwrap_or(i32::MAX)
    }

    fn get_indent(&self) -> i32 {
        self.indent_of(self.cur.lnum)
    }

    fn linewhite(&self, lnum: usize) -> bool {
        let l = self.line(lnum);
        at(l, skipwhite(l, 0)) == 0
    }

    // --- findmatchlimit (search.c), the parts cindent runs ---

    /// `findmatchlimit(NULL, initc, flags, maxtravel)` from the cursor. `initc` is `*` (a `/*` comment
    /// start, backward), `R` (a raw string start, backward), a bracket (its match: `{`, `(`, `[` look
    /// backward for the opener, `)` forward for the closer), or 0 (the bracket under or after the
    /// cursor). `blockstop`: stop at a `{` or `}` in column 0.
    fn findmatchlimit(&self, initc: u8, blockstop: bool, maxtravel: i64) -> Option<Pos> {
        let mut pos = self.cur;
        let mut linep = self.line(pos.lnum);
        let mut findc: u8 = 0;
        let mut initc = initc;
        let mut count: i64 = 0;
        let mut backwards;
        let mut raw_string = false;
        let mut inquote = false;
        let mut comment_dir = false; // only ever backward here
        let mut traveled: i64 = 0;
        let mut match_escaped = 0usize;
        let mut comment_col: Option<usize> = None;
        let pairs: [(u8, u8); 3] = [(b'(', b')'), (b'{', b'}'), (b'[', b']')];
        match initc {
            b'*' | b'R' => {
                comment_dir = true;
                backwards = true;
                raw_string = initc == b'R';
                initc = 0;
            }
            0 => {
                // The bracket under or after the cursor.
                if at(linep, pos.col) == 0 && pos.col > 0 {
                    pos.col -= 1;
                }
                backwards = false;
                loop {
                    let c = at(linep, pos.col);
                    if c == 0 {
                        break;
                    }
                    if let Some(&(o, cl)) = pairs.iter().find(|p| p.0 == c || p.1 == c) {
                        initc = c;
                        if c == o {
                            findc = cl;
                            backwards = false;
                        } else {
                            findc = o;
                            backwards = true;
                        }
                        break;
                    }
                    pos.col += char_len(linep, pos.col);
                }
                if findc == 0 {
                    return None;
                }
                let mut bslcnt = 0;
                let mut col = pos.col;
                while col > 0 && {
                    col -= 1;
                    col -= head_off(linep, col);
                    linep[col] == b'\\'
                } {
                    bslcnt += 1;
                }
                match_escaped = bslcnt & 1;
            }
            c => {
                // find_mps_values with switchit: look for the opener of `c` backward, or the closer
                // of `c` forward.
                let &(o, cl) = pairs.iter().find(|p| p.0 == c || p.1 == c)?;
                if c == o {
                    findc = o;
                    initc = cl;
                    backwards = true;
                } else {
                    findc = cl;
                    initc = o;
                    backwards = false;
                }
            }
        }
        let mut do_quotes: i32 = -1;
        let mut start_in_quotes: Option<bool> = None;
        let mut match_pos = Pos::default();
        if backwards && comment_dir {
            comment_col = check_linecomment(linep);
        }
        loop {
            if backwards {
                if pos.col == 0 {
                    if pos.lnum <= 1 {
                        break;
                    }
                    pos.lnum -= 1;
                    if maxtravel > 0 {
                        traveled += 1;
                        if traveled > maxtravel {
                            break;
                        }
                    }
                    linep = self.line(pos.lnum);
                    do_quotes = -1;
                    if comment_dir {
                        // The comment and raw-string searches only stop on their marks, which never
                        // cross a line end: a line without one is passed over at once (as the scan
                        // would, char by char).
                        let bit = if raw_string { MARK_RAW } else { MARK_COMMENT };
                        if !self.has_mark(pos.lnum, bit) {
                            pos.col = 0;
                            continue;
                        }
                        comment_col = check_linecomment(linep);
                    }
                    pos.col = strend(linep, 0);
                } else {
                    pos.col -= 1;
                    pos.col -= head_off(linep, pos.col);
                }
            } else {
                if at(linep, pos.col) == 0 {
                    if pos.lnum >= self.count() {
                        break;
                    }
                    pos.lnum += 1;
                    if maxtravel != 0 {
                        let t = traveled;
                        traveled += 1;
                        if t > maxtravel {
                            break;
                        }
                    }
                    linep = self.line(pos.lnum);
                    pos.col = 0;
                    do_quotes = -1;
                } else {
                    pos.col += char_len(linep, pos.col);
                }
            }
            if pos.col == 0 && blockstop && (at(linep, 0) == b'{' || at(linep, 0) == b'}') {
                if at(linep, 0) == findc && count == 0 {
                    return Some(pos);
                }
                break;
            }
            if comment_dir {
                if pos.col == 0 {
                    continue;
                } else if raw_string {
                    if at(linep, pos.col - 1) == b'R'
                        && at(linep, pos.col) == b'"'
                        && linep[pos.col + 1..strend(linep, 0)].contains(&b'(')
                    {
                        let end = if count > 0 { match_pos } else { self.cur };
                        if !self.find_rawstring_end(linep, pos, end) {
                            count += 1;
                            match_pos = pos;
                            match_pos.col -= 1;
                        }
                    }
                } else if at(linep, pos.col - 1) == b'/'
                    && at(linep, pos.col) == b'*'
                    && (pos.col == 1 || at(linep, pos.col - 2) != b'*')
                    && comment_col.is_none_or(|cc| pos.col < cc)
                {
                    count += 1;
                    match_pos = pos;
                    match_pos.col -= 1;
                } else if at(linep, pos.col - 1) == b'*' && at(linep, pos.col) == b'/' {
                    if count > 0 {
                        pos = match_pos;
                    } else if pos.col > 1
                        && at(linep, pos.col - 2) == b'/'
                        && comment_col.is_none_or(|cc| pos.col <= cc)
                    {
                        pos.col -= 2;
                    } else {
                        return None;
                    }
                    return Some(pos);
                }
                continue;
            }
            // Count the quotes in the line: brackets inside quotes are skipped when the count is even.
            if do_quotes == -1 {
                let mut at_start = do_quotes;
                let end = strend(linep, 0);
                let mut i = 0;
                while i < end {
                    if i == pos.col + usize::from(backwards) {
                        at_start = do_quotes & 1;
                    }
                    if linep[i] == b'"'
                        && (i == 0 || linep[i - 1] != b'\'' || at(linep, i + 1) != b'\'')
                    {
                        do_quotes += 1;
                    }
                    if linep[i] == b'\\' && at(linep, i + 1) != 0 {
                        i += 1;
                    }
                    i += 1;
                }
                do_quotes &= 1;
                if do_quotes == 0 {
                    inquote = false;
                    if end > 0 && linep[end - 1] == b'\\' {
                        do_quotes = 1;
                        if start_in_quotes.is_none() {
                            inquote = true;
                            start_in_quotes = Some(true);
                        } else if backwards {
                            inquote = true;
                        }
                    }
                    if pos.lnum > 1 {
                        let prev = self.line(pos.lnum - 1);
                        if last_byte(prev) == b'\\' {
                            do_quotes = 1;
                            if start_in_quotes.is_none() {
                                inquote = at_start != 0;
                                if inquote {
                                    start_in_quotes = Some(true);
                                }
                            } else if !backwards {
                                inquote = true;
                            }
                        }
                    }
                }
            }
            if start_in_quotes.is_none() {
                start_in_quotes = Some(false);
            }
            let c = at(linep, pos.col);
            match c {
                0 => {
                    if pos.col == 0 || linep[pos.col - 1] != b'\\' {
                        inquote = false;
                        start_in_quotes = Some(false);
                    }
                }
                b'"' => {
                    if do_quotes != 0 {
                        let n = linep[..pos.col]
                            .iter()
                            .rev()
                            .take_while(|&&b| b == b'\\')
                            .count();
                        if n % 2 == 0 {
                            inquote = !inquote;
                            start_in_quotes = Some(false);
                        }
                    }
                }
                _ => {
                    if c == b'\'' && initc != b'\'' && findc != b'\'' {
                        if backwards {
                            if pos.col > 1 {
                                if linep[pos.col - 2] == b'\'' {
                                    pos.col -= 2;
                                    continue;
                                } else if linep[pos.col - 2] == b'\\'
                                    && pos.col > 2
                                    && linep[pos.col - 3] == b'\''
                                {
                                    pos.col -= 3;
                                    continue;
                                }
                            }
                        } else if at(linep, pos.col + 1) != 0 {
                            if at(linep, pos.col + 1) == b'\\'
                                && at(linep, pos.col + 2) != 0
                                && at(linep, pos.col + 3) == b'\''
                            {
                                pos.col += 3;
                                continue;
                            } else if at(linep, pos.col + 2) == b'\'' {
                                pos.col += 2;
                                continue;
                            }
                        }
                    }
                    if (!inquote || start_in_quotes == Some(true)) && (c == initc || c == findc) {
                        let mut bslcnt = 0;
                        let mut col = pos.col;
                        while col > 0 && {
                            col -= 1;
                            col -= head_off(linep, col);
                            linep[col] == b'\\'
                        } {
                            bslcnt += 1;
                        }
                        if bslcnt & 1 == match_escaped {
                            if c == initc {
                                count += 1;
                            } else {
                                if count == 0 {
                                    return Some(pos);
                                }
                                count -= 1;
                            }
                        }
                    }
                }
            }
        }
        if comment_dir && count > 0 {
            return Some(match_pos);
        }
        None
    }

    /// `find_rawstring_end`: whether the raw string starting at `linep[start.col - 1]` ends before `end`.
    fn find_rawstring_end(&self, linep: &[u8], start: Pos, end: Pos) -> bool {
        let mut p = start.col + 1;
        while at(linep, p) != 0 && at(linep, p) != b'(' {
            p += 1;
        }
        let delim = &linep[start.col + 1..p];
        for lnum in start.lnum..=end.lnum {
            let line = self.line(lnum);
            let mut q = if lnum == start.lnum { start.col + 1 } else { 0 };
            while at(line, q) != 0 {
                if lnum == end.lnum && q >= end.col {
                    break;
                }
                if line[q] == b')'
                    && starts(line, q + 1, delim)
                    && at(line, q + delim.len() + 1) == b'"'
                {
                    return true;
                }
                q += 1;
            }
        }
        false
    }

    // --- indent_c.c helpers ---

    fn find_start_comment(&self, maxcomment: i32) -> Option<Pos> {
        let mut cur_max = i64::from(maxcomment);
        loop {
            let pos = self.findmatchlimit(b'*', false, cur_max)?;
            if !is_pos_in_string(self.line(pos.lnum), pos.col) {
                return Some(pos);
            }
            cur_max = self.cur.lnum as i64 - pos.lnum as i64 - 1;
            if cur_max <= 0 {
                return None;
            }
        }
    }

    fn find_start_rawstring(&self, maxcomment: i32) -> Option<Pos> {
        let mut cur_max = i64::from(maxcomment);
        loop {
            let pos = self.findmatchlimit(b'R', false, cur_max)?;
            if !is_pos_in_string(self.line(pos.lnum), pos.col) {
                return Some(pos);
            }
            cur_max = self.cur.lnum as i64 - pos.lnum as i64 - 1;
            if cur_max <= 0 {
                return None;
            }
        }
    }

    fn ind_find_start_comment(&self) -> Option<Pos> {
        self.find_start_comment(self.o().maxcomment)
    }

    /// `ind_find_start_CORS`: the start of the comment or raw string the cursor is in; `is_raw` gets
    /// the line of a raw string start.
    fn ind_find_start_cors(&self, is_raw: Option<&mut usize>) -> Option<Pos> {
        let comment_pos = self.find_start_comment(self.o().maxcomment);
        let rs_pos = self.find_start_rawstring(self.o().maxcomment);
        if comment_pos.is_none() || rs_pos.is_some_and(|r| Some(r) < comment_pos) {
            if let (Some(raw), Some(r)) = (is_raw, rs_pos) {
                *raw = r.lnum;
            }
            return rs_pos;
        }
        comment_pos
    }

    /// `cin_skipcomment`: past blanks and C comments (and `#` comments under `cino-#`).
    fn skipcomment(&self, s: &[u8], mut i: usize) -> usize {
        while at(s, i) != 0 {
            let prev = i;
            i = skipwhite(s, i);
            if self.o().hash_comment != 0 && i != prev && at(s, i) == b'#' {
                i = strend(s, i);
                break;
            }
            if at(s, i) != b'/' {
                break;
            }
            i += 1;
            if at(s, i) == b'/' {
                i = strend(s, i);
                break;
            }
            if at(s, i) != b'*' {
                break;
            }
            i += 1;
            while at(s, i) != 0 {
                if at(s, i) == b'*' && at(s, i + 1) == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
        }
        i
    }

    fn nocode(&self, s: &[u8], i: usize) -> bool {
        at(s, self.skipcomment(s, i)) == 0
    }

    fn find_line_comment(&self) -> Option<Pos> {
        let mut lnum = self.cur.lnum;
        while lnum > 1 {
            lnum -= 1;
            let line = self.line(lnum);
            let p = skipwhite(line, 0);
            if cin_islinecomment(line, p) {
                return Some(Pos { lnum, col: p });
            }
            if at(line, p) != 0 {
                break;
            }
        }
        None
    }

    fn cin_has_js_key(&self, s: &[u8], i: usize) -> bool {
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
    fn cin_islabel(&mut self) -> bool {
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

    fn cin_isinit(&self) -> bool {
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
    fn cin_iscase(&self, s: &[u8], i: usize, strict: bool) -> bool {
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

    fn cin_isscopedecl(&self, s: &[u8], i: usize) -> bool {
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

    fn cin_is_cpp_namespace(&self, s: &[u8], i: usize) -> bool {
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
    fn after_label(&self, l: &[u8]) -> Option<usize> {
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

    fn get_indent_nolabel(&self, lnum: usize) -> i32 {
        let l = self.line(lnum);
        match self.after_label(l) {
            Some(p) => i32::try_from(self.vcol(lnum, p)).unwrap_or(i32::MAX),
            None => 0,
        }
    }

    /// `skip_label`: the indent of line `lnum`, ignoring a case or jump label, and where its text
    /// (after the label) starts.
    fn skip_label(&mut self, lnum: usize) -> (i32, &'a [u8], usize) {
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

    fn cin_first_id_amount(&self) -> i32 {
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

    fn cin_get_equal_amount(&self, lnum: usize) -> i32 {
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
    fn cin_ispreproc_cont(&self, l: &mut &'a [u8], lnum: &mut usize, amount: &mut i32) -> bool {
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
    fn isterminated(&self, s: &[u8], i: usize, incl_open: bool, incl_comma: bool) -> u8 {
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

    fn cin_iselse(&self, s: &[u8], mut i: usize) -> bool {
        if at(s, i) == b'}' {
            i = self.skipcomment(s, i + 1);
        }
        starts(s, i, b"else") && !is_idc(at(s, i + 4))
    }

    /// `cin_isfuncdecl`: whether the line at `first_lnum` (or `sp`) looks like a function declaration:
    /// a `(` and a `)` ending the line (or a later one, after `,`), no `;` before.
    fn cin_isfuncdecl(&mut self, sp: Option<&'a [u8]>, first_lnum: usize, min_lnum: usize) -> bool {
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
    fn cin_iswhileofdo(&mut self, s: &[u8], i: usize, lnum: usize) -> bool {
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

    fn cin_is_if_for_while_before_offset(line: &[u8], poffset: &mut usize) -> bool {
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
    fn cin_iswhileofdo_end(&mut self, terminated: u8) -> bool {
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
    fn cin_is_cpp_baseclass(&mut self, cache: &mut (bool, Pos)) -> bool {
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
            } else if (starts(line, s, b"class") && !is_idc(at(line, s + 5)))
                || (starts(line, s, b"struct") && !is_idc(at(line, s + 6)))
            {
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

    fn get_baseclass_amount(&mut self, col: usize) -> i32 {
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
    fn cin_ends_in(&self, s: &[u8], find: &[u8]) -> bool {
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

    fn cin_is_cpp_extern_c(&self, s: &[u8], i: usize) -> bool {
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
    fn cin_skip2pos(&self, pos: Pos) -> usize {
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

    /// `find_start_brace`: the `{` that opens the block the cursor is in, ignoring one in a comment.
    fn find_start_brace(&mut self) -> Option<Pos> {
        let save = self.cur;
        let mut found = None;
        while let Some(t) = self.findmatchlimit(b'{', true, 0) {
            self.cur = t;
            let mut pos = None;
            if self.cin_skip2pos(t) == t.col {
                pos = self.ind_find_start_cors(None);
                if pos.is_none() {
                    found = Some(t);
                    break;
                }
            }
            if let Some(p) = pos {
                self.cur = p;
            }
        }
        self.cur = save;
        found
    }

    fn find_match_paren(&mut self, maxparen: i32) -> Option<Pos> {
        self.find_match_char(b'(', maxparen)
    }

    /// `find_match_char`: the unmatched `c` before the cursor, ignoring one in a comment.
    fn find_match_char(&mut self, c: u8, maxparen: i32) -> Option<Pos> {
        let save = self.cur;
        let mut maxp = maxparen;
        let mut result = None;
        while let Some(t) = self.findmatchlimit(c, false, i64::from(maxp)) {
            if self.cin_skip2pos(t) > t.col {
                maxp = maxparen - (save.lnum as i32 - t.lnum as i32);
                if maxp > 0 {
                    self.cur = t;
                    self.cur.col = 0;
                    continue;
                }
                break;
            }
            self.cur = t;
            if let Some(w) = self.ind_find_start_cors(None) {
                maxp = maxparen - (save.lnum as i32 - w.lnum as i32);
                if maxp > 0 {
                    self.cur = w;
                    continue;
                }
                break;
            }
            result = Some(t);
            break;
        }
        self.cur = save;
        result
    }

    fn find_match_paren_after_brace(&mut self, maxparen: i32) -> Option<Pos> {
        let t = self.find_match_paren(maxparen)?;
        match self.find_start_brace() {
            Some(b) if (t.lnum, t.col) < (b.lnum, b.col) => None,
            _ => Some(t),
        }
    }

    fn corr_ind_maxparen(&self, start: Pos) -> i32 {
        let n = start.lnum as i64 - self.cur.lnum as i64;
        let mp = self.o().maxparen;
        if n > 0 && n < i64::from(mp / 2) {
            return mp - n as i32;
        }
        mp
    }

    /// `find_last_paren`: the cursor column goes to the last unmatched `end` of `l` (0 when none).
    fn find_last_paren(&mut self, l: &[u8], start: u8, end: u8) -> bool {
        let mut retval = false;
        let mut open = 0;
        self.cur.col = 0;
        let mut i = 0;
        while at(l, i) != 0 {
            i = self.skipcomment(l, i);
            i = skip_string(l, i);
            if at(l, i) == start {
                open += 1;
            } else if at(l, i) == end {
                if open > 0 {
                    open -= 1;
                } else {
                    self.cur.col = i;
                    retval = true;
                }
            }
            i += 1;
        }
        retval
    }

    fn cin_is_cinword(&self, s: &[u8], i: usize) -> bool {
        let i = skipwhite(s, i);
        parts(self.e.cinwords).any(|w| {
            starts(s, i, w) && (!is_wordc(at(s, i + w.len())) || !is_wordc(w[w.len() - 1]))
        })
    }

    /// `find_match`: from the cursor, the `if` an `else` belongs to (`LOOKFOR_IF`) or the `do` of a
    /// `while` (`LOOKFOR_DO`) in scope `ourscope`; the cursor rests on it.
    fn find_match(&mut self, lookfor_if: bool, ourscope: usize) -> bool {
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

    /// `in_cinkeys` for the cursor line: whether `'cinkeys'` holds the key `keytyped` for the moment
    /// `when` (`*` before inserting, `!` instead of inserting, ` ` after inserting). `line_is_empty`
    /// accepts the keys with a `0` before them.
    pub(crate) fn in_cinkeys(&mut self, keytyped: CinKey, when: u8, line_is_empty: bool) -> bool {
        let look = self.e.cinkeys.as_bytes();
        let mut i = 0;
        while i < look.len() {
            let mut try_match = match when {
                b'*' => look[i] == b'*',
                b'!' => look[i] == b'!',
                _ => look[i] != b'*',
            };
            if look[i] == b'*' || look[i] == b'!' {
                i += 1;
            }
            let try_match_word;
            if at(look, i) == b'0' {
                try_match_word = try_match;
                if !line_is_empty {
                    try_match = false;
                }
                i += 1;
            } else {
                try_match_word = false;
            }
            let c = at(look, i);
            if c == b'^' && (b'?'..=b'_').contains(&at(look, i + 1)) {
                let ctrl = at(look, i + 1) ^ 0x40;
                if try_match && keytyped == CinKey::Char(ctrl) {
                    return true;
                }
                i += 2;
            } else if c == b'o' {
                if try_match && keytyped == CinKey::OpenForw {
                    return true;
                }
                i += 1;
            } else if c == b'O' {
                if try_match && keytyped == CinKey::OpenBack {
                    return true;
                }
                i += 1;
            } else if c == b'e' {
                if try_match && keytyped == CinKey::Char(b'e') && self.cur.col >= 4 {
                    let p = self.cur_line();
                    let c0 = self.cur.col - 4;
                    if skipwhite(p, 0) == c0 && starts(p, c0, b"else") {
                        return true;
                    }
                }
                i += 1;
            } else if c == b':' {
                if try_match && keytyped == CinKey::Char(b':') {
                    let p = self.cur_line();
                    if self.cin_iscase(p, 0, false)
                        || self.cin_isscopedecl(p, 0)
                        || self.cin_islabel()
                    {
                        return true;
                    }
                    let col = self.cur.col;
                    if col > 2 && at(p, col - 1) == b':' && at(p, col - 2) == b':' {
                        // As if the `:` just typed were a blank (`class::method` was a label before it).
                        let mut q = p.to_vec();
                        q[col - 1] = b' ';
                        let one = [q];
                        let sub = Ci {
                            lines: &one[..],
                            e: self.e,
                            cur: Pos { lnum: 1, col },
                            insert: self.insert,
                            marks: None,
                        };
                        // The label check reads the lines above: splice the line into the buffer.
                        let mut all: Vec<Vec<u8>> =
                            self.lines.iter().map(|l| l.as_ref().to_vec()).collect();
                        all[self.cur.lnum - 1].clone_from(&one[0]);
                        let hit = sub.cin_iscase(&one[0], 0, false)
                            || sub.cin_isscopedecl(&one[0], 0)
                            || {
                                let mut full = Ci {
                                    lines: &all[..],
                                    e: self.e,
                                    cur: self.cur,
                                    insert: self.insert,
                                    marks: None,
                                };
                                full.cin_islabel()
                            };
                        if hit {
                            return true;
                        }
                    }
                }
                i += 1;
            } else if c == b'<' {
                if try_match {
                    let n = at(look, i + 1);
                    if b"<>!*oOe0:".contains(&n) && n != 0 && keytyped == CinKey::Char(n) {
                        return true;
                    }
                    if let Some(k) = special_key(&look[i..]) {
                        if keytyped == k {
                            return true;
                        }
                    }
                }
                while at(look, i) != 0 && at(look, i) != b'>' {
                    i += 1;
                }
                while at(look, i) == b'>' {
                    i += 1;
                }
            } else if c == b'=' && at(look, i + 1) != b',' && at(look, i + 1) != 0 {
                i += 1;
                let icase = at(look, i) == b'~';
                if icase {
                    i += 1;
                }
                let end = look[i..]
                    .iter()
                    .position(|&b| b == b',')
                    .map_or(look.len(), |k| i + k);
                let word = &look[i..end];
                let col = self.cur.col;
                if (try_match || try_match_word) && col >= word.len() && !word.is_empty() {
                    let last = word[word.len() - 1];
                    let eq = |a: u8, b: u8| {
                        if icase {
                            a.eq_ignore_ascii_case(&b)
                        } else {
                            a == b
                        }
                    };
                    let mut matched = false;
                    if let CinKey::Char(k) = keytyped {
                        if eq(k, last) {
                            let line = self.cur_line();
                            let ws = col - word.len();
                            if (ws == 0 || !is_wordc(at(line, ws - 1)))
                                && line.len() >= col
                                && line[ws..col].iter().zip(word).all(|(&a, &b)| eq(a, b))
                            {
                                matched = true;
                            }
                        }
                    }
                    if matched && try_match_word && !try_match {
                        let line = self.cur_line();
                        if skipwhite(line, 0) != col - word.len() {
                            matched = false;
                        }
                    }
                    if matched {
                        return true;
                    }
                }
                i = end;
            } else {
                if try_match && CinKey::Char(c) == keytyped {
                    return true;
                }
                if c != 0 {
                    i += 1;
                }
            }
            // skip_to_option_part: past the `,` and blanks.
            if at(look, i) == b',' {
                i += 1;
            }
            while at(look, i) == b' ' {
                i += 1;
            }
        }
        false
    }

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

/// A key `'cinkeys'` can name: a typed byte, or the `o`/`O` opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CinKey {
    /// A typed byte (a control key as its byte, `^F` is 6).
    Char(u8),
    /// A line opened below (`o`, `<CR>`).
    OpenForw,
    /// A line opened above (`O`).
    OpenBack,
}

/// The key a `<name>` in `'cinkeys'` stands for, for the few names that are one byte.
fn special_key(s: &[u8]) -> Option<CinKey> {
    let end = s.iter().position(|&c| c == b'>')?;
    let name = &s[1..end];
    let k = match name.to_ascii_lowercase().as_slice() {
        b"tab" => b'\t',
        b"cr" | b"enter" | b"return" => b'\r',
        b"space" => b' ',
        b"bar" => b'|',
        b"lt" => b'<',
        b"bslash" => b'\\',
        b"bs" => 8,
        b"esc" => 27,
        [b'c', b'-', c] if c.is_ascii_alphabetic() => c.to_ascii_uppercase() ^ 0x40,
        _ => return None,
    };
    Some(CinKey::Char(k))
}

/// The C indent of line `row` (0-based) of `lines`, as `get_c_indent` gives it with the cursor at byte
/// `col` (in Insert mode a `)` there is cut off first). `None` keeps the line (inside a raw string).
pub(crate) fn line_indent<L: AsRef<[u8]>>(
    lines: &[L],
    row: usize,
    col: usize,
    insert: bool,
    e: &CEnv<'_>,
) -> Option<usize> {
    let marks: Vec<u8> = lines.iter().map(|l| line_marks(l.as_ref())).collect();
    marked_line_indent(lines, &marks, row, col, insert, e)
}

/// [`line_indent`] with the lines' [`line_marks`] worked out already.
fn marked_line_indent<L: AsRef<[u8]>>(
    lines: &[L],
    marks: &[u8],
    row: usize,
    col: usize,
    insert: bool,
    e: &CEnv<'_>,
) -> Option<usize> {
    let mut ci = Ci::new(lines, e);
    ci.marks = Some(marks);
    let n = ci.get_c_indent(row + 1, col, insert);
    usize::try_from(n).ok()
}

/// `=` over lines `[first, last]` (0-based) of `b`, as nvim's `op_reindent` runs `get_c_indent`: top
/// down, each line seeing the lines above it already re-indented. A blank line is emptied; `None` keeps
/// a line. (The engine reads indents as columns, so the re-indented lines above are kept as spaces.)
pub(crate) fn reindent(b: &[u8], first: usize, last: usize, e: &CEnv<'_>) -> Vec<Option<usize>> {
    let mut lines: Vec<Vec<u8>> = b.split(|&c| c == b'\n').map(<[u8]>::to_vec).collect();
    let last = last.min(lines.len().saturating_sub(1));
    let mut out = Vec::with_capacity(last + 1 - first.min(last + 1));
    // A re-indent changes only blanks, so the marks hold for the whole run.
    let marks: Vec<u8> = lines.iter().map(|l| line_marks(l)).collect();
    for l in first..=last {
        let fnb = skipwhite(&lines[l], 0);
        let want = if fnb >= lines[l].len() {
            Some(0)
        } else {
            marked_line_indent(&lines, &marks, l, 0, false, e)
        };
        if let Some(c) = want {
            let mut line = vec![b' '; c];
            line.extend_from_slice(&lines[l][fnb..]);
            lines[l] = line;
        }
        out.push(want);
    }
    out
}

/// Whether `'cinkeys'` re-indents the caret's line after `key` was typed (or a line was opened), the
/// caret at byte `col` of line `row` (0-based); `line_is_empty`: only blanks before the key.
pub(crate) fn key_reindents<L: AsRef<[u8]>>(
    lines: &[L],
    row: usize,
    col: usize,
    key: CinKey,
    line_is_empty: bool,
    e: &CEnv<'_>,
) -> bool {
    let mut ci = Ci::new(lines, e);
    ci.cur = Pos { lnum: row + 1, col };
    ci.insert = true;
    ci.in_cinkeys(key, b' ', line_is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(cino: &str) -> CEnv<'static> {
        let cino: &'static str = Box::leak(cino.to_string().into_boxed_str());
        CEnv {
            ts: crate::cells::Tabs::uniform(8),
            cino: CinOptions::parse(cino, 4),
            cinwords: "if,else,while,do,for,switch",
            cinscopedecls: DEFAULT_CINSCOPEDECLS,
            cinkeys: DEFAULT_CINKEYS,
            comments: "s1:/*,mb:*,ex:*/,://,b:#,:%,:XCOMM,n:>,fb:-",
        }
    }

    fn ind(src: &str, row: usize, cino: &str) -> Option<usize> {
        let lines: Vec<&[u8]> = src.split('\n').map(str::as_bytes).collect();
        line_indent(&lines, row, 0, false, &env(cino))
    }

    #[test]
    fn parses_cinoptions() {
        let o = CinOptions::parse(">2s,e-1s,n.5s,:0,(0,L0", 4);
        assert_eq!(o.level, 8);
        assert_eq!(o.open_imag, -4);
        assert_eq!(o.no_brace, 2);
        assert_eq!(o.case, 0);
        assert_eq!(o.unclosed, 0);
        assert_eq!(o.jump_label, 0);
        let d = CinOptions::parse("", 4);
        assert_eq!(
            (d.level, d.unclosed, d.maxparen, d.maxcomment),
            (4, 8, 20, 70)
        );
    }

    #[test]
    fn basic_c_layout() {
        assert_eq!(ind("int f() {\nx;", 1, ""), Some(4));
        assert_eq!(ind("int f() {\n    if (a)\nx;", 2, ""), Some(8));
        assert_eq!(ind("int f() {\n    if (a)\n        x;\ny;", 3, ""), Some(4));
        assert_eq!(ind("int f() {\n    x = a\n? b", 2, ""), Some(8));
        assert_eq!(ind("int f() {\n    g(a,\nb", 2, ""), Some(12));
        assert_eq!(ind("int f() {\n    x;\n}", 2, ""), Some(0));
        assert_eq!(ind("int f() {\n    switch (a) {\ncase 1:", 2, ""), Some(8));
        assert_eq!(
            ind("int f() {\n    switch (a) {\ncase 1:", 2, ":0"),
            Some(4)
        );
        assert_eq!(ind("int f() {\nout:", 1, ""), Some(0));
        assert_eq!(ind("int f() {\n#if A", 1, ""), Some(0));
        assert_eq!(ind("int f() {\n    /* a\nb", 2, ""), Some(7));
        assert_eq!(ind("int f() {\n    /*\n*", 2, ""), Some(5));
        // A top-level `{` takes cino-f (0): the object `{` JavaScript indents is not cindent's.
        assert_eq!(ind("var o =\n{", 1, ""), Some(0));
    }

    /// A C file of `n`-odd lines of the usual shape (functions, blocks, comments, a switch,
    /// continuation lines), unindented.
    fn big_c(n: usize) -> String {
        let unit = "/* f does x\n * and y */\nint f(int a,\nint b)\n{\nint x = a +\nb;\nif (a) {\n\
x++; // inc\n} else {\nx--;\n}\nswitch (x) {\ncase 1:\nbreak;\ndefault:\nx = g(a,\nb);\n}\n\
while (x)\nx--;\nreturn x;\n}\n\n";
        let per = unit.matches('\n').count();
        unit.repeat(n / per + 1)
    }

    /// `=G` over a 10k-line C file (#1163):
    /// `cargo test -p ruse-core cindent_eq_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing probe"]
    #[allow(clippy::print_stdout)] // the timings are the output
    fn cindent_eq_cost() {
        let src = big_c(10_000);
        let lines = src.matches('\n').count();
        let e = env("");
        let t = std::time::Instant::now();
        let cols = reindent(src.as_bytes(), 0, lines - 1, &e);
        println!("=G over {lines} lines: {:?}", t.elapsed());
        // One key or open at the end (the per-key path splits the buffer and marks its lines).
        let ls: Vec<&[u8]> = src.split('\n').map(str::as_bytes).collect();
        let t = std::time::Instant::now();
        let _ = line_indent(&ls, lines - 3, 0, true, &e);
        println!("one line at the end: {:?}", t.elapsed());
        assert_eq!(cols[5], Some(4)); // `int x = a +`
        assert_eq!(cols[6], Some(8)); // its continuation
    }
}
