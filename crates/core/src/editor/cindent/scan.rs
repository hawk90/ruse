//! The searches over lines the C indent runs: `findmatchlimit` (matching brackets, `/*` comment
//! and raw-string starts), and the start-brace, matching-paren and last-paren searches.

use super::lex::{
    at, char_len, check_linecomment, cin_islinecomment, head_off, is_pos_in_string, last_byte,
    skip_string, skipwhite, starts, strend, MARK_COMMENT, MARK_RAW,
};
use super::{Ci, Pos};

/// The state `findmatchlimit` carries from one phase to the next: the target-char selection sets the
/// direction and the chars to count, the scan loop moves `pos` and keeps the quote and comment state.
struct MatchScan<'a> {
    /// The current search position (nvim's static `pos`).
    pos: Pos,
    /// The text of line `pos.lnum`.
    linep: &'a [u8],
    /// The char whose match is wanted (counted up), 0 for a comment or raw-string search.
    initc: u8,
    /// The matching char (counted down), 0 for a comment or raw-string search.
    findc: u8,
    /// The cumulative number of unmatched `initc`s (or comment and raw-string starts).
    count: i64,
    backwards: bool,
    /// Searching for a raw string start (`initc` `R`).
    raw_string: bool,
    /// True when inside quotes.
    inquote: bool,
    /// Searching for a comment or raw-string start; only ever backward here.
    comment_dir: bool,
    /// How many lines the search has gone so far.
    traveled: i64,
    /// 1 when the match wanted is escaped (an odd number of backslashes before it).
    match_escaped: usize,
    /// The start of a `//` comment on the line, if any.
    comment_col: Option<usize>,
    /// The parity of the quotes on the line, -1 until counted.
    do_quotes: i32,
    /// Whether the search started inside quotes, `None` until known.
    start_in_quotes: Option<bool>,
    /// The last comment or raw-string start found.
    match_pos: Pos,
}

/// What a phase of one `findmatchlimit` loop iteration decides.
enum MatchStep {
    /// Go on to the next phase of this iteration.
    Go,
    /// The iteration is over: go to the next position (nvim's `continue`).
    Next,
    /// Leave the loop (nvim's `break`).
    Break,
    /// Return this result from `findmatchlimit` at once.
    Return(Option<Pos>),
}

/// The number of backslashes right before `linep[col]`.
fn backslashes_before(linep: &[u8], col: usize) -> usize {
    let mut bslcnt = 0;
    let mut col = col;
    while col > 0 && {
        col -= 1;
        col -= head_off(linep, col);
        linep[col] == b'\\'
    } {
        bslcnt += 1;
    }
    bslcnt
}

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// `findmatchlimit(NULL, initc, flags, maxtravel)` from the cursor. `initc` is `*` (a `/*` comment
    /// start, backward), `R` (a raw string start, backward), a bracket (its match: `{`, `(`, `[` look
    /// backward for the opener, `)` forward for the closer), or 0 (the bracket under or after the
    /// cursor). `blockstop`: stop at a `{` or `}` in column 0.
    ///
    /// The phases run in nvim's order: the target-char selection, the `//` comment column of the
    /// first line, then one scan step per position until a phase returns or breaks; a comment or
    /// raw-string search that ran out returns the last start it found.
    pub(super) fn findmatchlimit(&self, initc: u8, blockstop: bool, maxtravel: i64) -> Option<Pos> {
        let mut s = self.fml_target(initc)?;
        if s.backwards && s.comment_dir {
            s.comment_col = check_linecomment(s.linep);
        }
        loop {
            match self.fml_step(&mut s, blockstop, maxtravel) {
                MatchStep::Go | MatchStep::Next => {}
                MatchStep::Break => break,
                MatchStep::Return(found) => return found,
            }
        }
        if s.comment_dir && s.count > 0 {
            return Some(s.match_pos);
        }
        None
    }

    /// `findmatchlimit`, "if initc given, look in the table for the matching character" ("'/' and
    /// '*' are special cases: look for start or end of comment") and "Find the brace under or after
    /// the cursor" with "Set "match_escaped" if there are an odd number of backslashes". `None`: no
    /// bracket to match (nvim's `return NULL`).
    fn fml_target(&self, initc: u8) -> Option<MatchScan<'a>> {
        let mut pos = self.cur;
        let linep = self.line(pos.lnum);
        let mut findc: u8 = 0;
        let mut initc = initc;
        let backwards;
        let mut raw_string = false;
        let mut comment_dir = false; // only ever backward here
        let mut match_escaped = 0usize;
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
                let mut back = false;
                loop {
                    let c = at(linep, pos.col);
                    if c == 0 {
                        break;
                    }
                    if let Some(&(o, cl)) = pairs.iter().find(|p| p.0 == c || p.1 == c) {
                        initc = c;
                        if c == o {
                            findc = cl;
                            back = false;
                        } else {
                            findc = o;
                            back = true;
                        }
                        break;
                    }
                    pos.col += char_len(linep, pos.col);
                }
                backwards = back;
                if findc == 0 {
                    return None;
                }
                match_escaped = backslashes_before(linep, pos.col) & 1;
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
        Some(MatchScan {
            pos,
            linep,
            initc,
            findc,
            count: 0,
            backwards,
            raw_string,
            inquote: false,
            comment_dir,
            traveled: 0,
            match_escaped,
            comment_col: None,
            do_quotes: -1,
            start_in_quotes: None,
            match_pos: Pos::default(),
        })
    }

    /// One iteration of the `findmatchlimit` scan loop (`while (!got_int)`): its phases in nvim's
    /// order, each going on to the next only on [`MatchStep::Go`].
    fn fml_step(&self, s: &mut MatchScan<'a>, blockstop: bool, maxtravel: i64) -> MatchStep {
        let step = self.fml_advance(s, maxtravel);
        if !matches!(step, MatchStep::Go) {
            return step;
        }
        let step = fml_blockstop(s, blockstop);
        if !matches!(step, MatchStep::Go) {
            return step;
        }
        if s.comment_dir {
            return self.fml_comment(s);
        }
        self.fml_count_quotes(s);
        if s.start_in_quotes.is_none() {
            s.start_in_quotes = Some(false);
        }
        fml_match_char(s)
    }

    /// `findmatchlimit`, "Go to the next position, forward or backward": the previous or next char,
    /// crossing to the next line at a line end, until the buffer edge or `maxtravel` lines.
    fn fml_advance(&self, s: &mut MatchScan<'a>, maxtravel: i64) -> MatchStep {
        if s.backwards {
            if s.pos.col == 0 {
                if s.pos.lnum <= 1 {
                    return MatchStep::Break;
                }
                s.pos.lnum -= 1;
                if maxtravel > 0 {
                    s.traveled += 1;
                    if s.traveled > maxtravel {
                        return MatchStep::Break;
                    }
                }
                s.linep = self.line(s.pos.lnum);
                s.do_quotes = -1;
                if s.comment_dir {
                    // The comment and raw-string searches only stop on their marks, which never
                    // cross a line end: a line without one is passed over at once (as the scan
                    // would, char by char).
                    let bit = if s.raw_string { MARK_RAW } else { MARK_COMMENT };
                    if !self.has_mark(s.pos.lnum, bit) {
                        s.pos.col = 0;
                        return MatchStep::Next;
                    }
                    s.comment_col = check_linecomment(s.linep);
                }
                s.pos.col = strend(s.linep, 0);
            } else {
                s.pos.col -= 1;
                s.pos.col -= head_off(s.linep, s.pos.col);
            }
        } else if at(s.linep, s.pos.col) == 0 {
            if s.pos.lnum >= self.count() {
                return MatchStep::Break;
            }
            s.pos.lnum += 1;
            if maxtravel != 0 {
                let t = s.traveled;
                s.traveled += 1;
                if t > maxtravel {
                    return MatchStep::Break;
                }
            }
            s.linep = self.line(s.pos.lnum);
            s.pos.col = 0;
            s.do_quotes = -1;
        } else {
            s.pos.col += char_len(s.linep, s.pos.col);
        }
        MatchStep::Go
    }

    /// `findmatchlimit`, the `comment_dir` branch: "Note: comments do not nest, and we ignore quotes
    /// in them", with "Possible start of raw string" for `R` and "A comment may contain / * or / /, it
    /// may also start or end with / * /. Ignore a / * after / / and after *." for `*`. A comment end
    /// returns the start found (or the `/*` it closes); anything else goes to the next position.
    fn fml_comment(&self, s: &mut MatchScan<'a>) -> MatchStep {
        let linep = s.linep;
        let pos = s.pos;
        if pos.col == 0 {
            return MatchStep::Next;
        }
        if s.raw_string {
            if at(linep, pos.col - 1) == b'R'
                && at(linep, pos.col) == b'"'
                && linep[pos.col + 1..strend(linep, 0)].contains(&b'(')
            {
                let end = if s.count > 0 { s.match_pos } else { self.cur };
                if !self.find_rawstring_end(linep, pos, end) {
                    s.count += 1;
                    s.match_pos = pos;
                    s.match_pos.col -= 1;
                }
            }
        } else if at(linep, pos.col - 1) == b'/'
            && at(linep, pos.col) == b'*'
            && (pos.col == 1 || at(linep, pos.col - 2) != b'*')
            && s.comment_col.is_none_or(|cc| pos.col < cc)
        {
            s.count += 1;
            s.match_pos = pos;
            s.match_pos.col -= 1;
        } else if at(linep, pos.col - 1) == b'*' && at(linep, pos.col) == b'/' {
            let found = if s.count > 0 {
                s.match_pos
            } else if pos.col > 1
                && at(linep, pos.col - 2) == b'/'
                && s.comment_col.is_none_or(|cc| pos.col <= cc)
            {
                Pos {
                    col: pos.col - 2,
                    ..pos
                }
            } else {
                return MatchStep::Return(None);
            };
            return MatchStep::Return(Some(found));
        }
        MatchStep::Next
    }

    /// `findmatchlimit`, "If smart matching ('cpoptions' does not contain '%'), braces inside of
    /// quotes are ignored, but only if there is an even number of quotes in the line" with "If we
    /// find an uneven count, check current line and previous one for a '\' at the end". Runs once
    /// per line (`do_quotes == -1`).
    fn fml_count_quotes(&self, s: &mut MatchScan<'a>) {
        if s.do_quotes != -1 {
            return;
        }
        let linep = s.linep;
        let mut at_start = s.do_quotes;
        let end = strend(linep, 0);
        let mut i = 0;
        while i < end {
            if i == s.pos.col + usize::from(s.backwards) {
                at_start = s.do_quotes & 1;
            }
            if linep[i] == b'"' && (i == 0 || linep[i - 1] != b'\'' || at(linep, i + 1) != b'\'') {
                s.do_quotes += 1;
            }
            if linep[i] == b'\\' && at(linep, i + 1) != 0 {
                i += 1;
            }
            i += 1;
        }
        s.do_quotes &= 1;
        if s.do_quotes == 0 {
            s.inquote = false;
            if end > 0 && linep[end - 1] == b'\\' {
                s.do_quotes = 1;
                if s.start_in_quotes.is_none() {
                    s.inquote = true;
                    s.start_in_quotes = Some(true);
                } else if s.backwards {
                    s.inquote = true;
                }
            }
            if s.pos.lnum > 1 {
                let prev = self.line(s.pos.lnum - 1);
                if last_byte(prev) == b'\\' {
                    s.do_quotes = 1;
                    if s.start_in_quotes.is_none() {
                        s.inquote = at_start != 0;
                        if s.inquote {
                            s.start_in_quotes = Some(true);
                        }
                    } else if !s.backwards {
                        s.inquote = true;
                    }
                }
            }
        }
    }
}

/// `findmatchlimit`, "If FM_BLOCKSTOP given, stop at a '{' or '}' in column 0": the match when it is
/// that brace, else the end of the search.
fn fml_blockstop(s: &MatchScan<'_>, blockstop: bool) -> MatchStep {
    let c0 = at(s.linep, 0);
    if s.pos.col == 0 && blockstop && (c0 == b'{' || c0 == b'}') {
        if c0 == s.findc && s.count == 0 {
            return MatchStep::Return(Some(s.pos));
        }
        return MatchStep::Break;
    }
    MatchStep::Go
}

/// `findmatchlimit`, the `switch (c = ...)` on the char at `pos`: "If 'smartmatch' is set: Things
/// inside quotes are ignored by setting 'inquote'", "Skip things in single quotes: 'x' or '\x'", and
/// "Check for match outside of quotes, and inside of quotes when the start is also inside of quotes"
/// with "Only accept a match when ... escaping is what we expect". Returns the match when `count`
/// is back at 0.
fn fml_match_char(s: &mut MatchScan<'_>) -> MatchStep {
    let linep = s.linep;
    let c = at(linep, s.pos.col);
    match c {
        0 => {
            if s.pos.col == 0 || linep[s.pos.col - 1] != b'\\' {
                s.inquote = false;
                s.start_in_quotes = Some(false);
            }
        }
        b'"' => {
            if s.do_quotes != 0 {
                let n = linep[..s.pos.col]
                    .iter()
                    .rev()
                    .take_while(|&&b| b == b'\\')
                    .count();
                if n % 2 == 0 {
                    s.inquote = !s.inquote;
                    s.start_in_quotes = Some(false);
                }
            }
        }
        _ => {
            if c == b'\'' && s.initc != b'\'' && s.findc != b'\'' {
                if s.backwards {
                    if s.pos.col > 1 {
                        if linep[s.pos.col - 2] == b'\'' {
                            s.pos.col -= 2;
                            return MatchStep::Next;
                        } else if linep[s.pos.col - 2] == b'\\'
                            && s.pos.col > 2
                            && linep[s.pos.col - 3] == b'\''
                        {
                            s.pos.col -= 3;
                            return MatchStep::Next;
                        }
                    }
                } else if at(linep, s.pos.col + 1) != 0 {
                    if at(linep, s.pos.col + 1) == b'\\'
                        && at(linep, s.pos.col + 2) != 0
                        && at(linep, s.pos.col + 3) == b'\''
                    {
                        s.pos.col += 3;
                        return MatchStep::Next;
                    } else if at(linep, s.pos.col + 2) == b'\'' {
                        s.pos.col += 2;
                        return MatchStep::Next;
                    }
                }
            }
            if (!s.inquote || s.start_in_quotes == Some(true)) && (c == s.initc || c == s.findc) {
                let bslcnt = backslashes_before(linep, s.pos.col);
                if bslcnt & 1 == s.match_escaped {
                    if c == s.initc {
                        s.count += 1;
                    } else {
                        if s.count == 0 {
                            return MatchStep::Return(Some(s.pos));
                        }
                        s.count -= 1;
                    }
                }
            }
        }
    }
    MatchStep::Next
}

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
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

    pub(super) fn find_start_rawstring(&self, maxcomment: i32) -> Option<Pos> {
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

    pub(super) fn ind_find_start_comment(&self) -> Option<Pos> {
        self.find_start_comment(self.o().maxcomment)
    }

    /// `ind_find_start_CORS`: the start of the comment or raw string the cursor is in; `is_raw` gets
    /// the line of a raw string start.
    pub(super) fn ind_find_start_cors(&self, is_raw: Option<&mut usize>) -> Option<Pos> {
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
    pub(super) fn skipcomment(&self, s: &[u8], mut i: usize) -> usize {
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

    pub(super) fn nocode(&self, s: &[u8], i: usize) -> bool {
        at(s, self.skipcomment(s, i)) == 0
    }

    pub(super) fn find_line_comment(&self) -> Option<Pos> {
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

    /// `find_start_brace`: the `{` that opens the block the cursor is in, ignoring one in a comment.
    pub(super) fn find_start_brace(&mut self) -> Option<Pos> {
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

    pub(super) fn find_match_paren(&mut self, maxparen: i32) -> Option<Pos> {
        self.find_match_char(b'(', maxparen)
    }

    /// `find_match_char`: the unmatched `c` before the cursor, ignoring one in a comment.
    pub(super) fn find_match_char(&mut self, c: u8, maxparen: i32) -> Option<Pos> {
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

    pub(super) fn find_match_paren_after_brace(&mut self, maxparen: i32) -> Option<Pos> {
        let t = self.find_match_paren(maxparen)?;
        match self.find_start_brace() {
            Some(b) if (t.lnum, t.col) < (b.lnum, b.col) => None,
            _ => Some(t),
        }
    }

    pub(super) fn corr_ind_maxparen(&self, start: Pos) -> i32 {
        let n = start.lnum as i64 - self.cur.lnum as i64;
        let mp = self.o().maxparen;
        if n > 0 && n < i64::from(mp / 2) {
            return mp - n as i32;
        }
        mp
    }

    /// `find_last_paren`: the cursor column goes to the last unmatched `end` of `l` (0 when none).
    pub(super) fn find_last_paren(&mut self, l: &[u8], start: u8, end: u8) -> bool {
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
}
