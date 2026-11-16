//! The searches over lines the C indent runs: `findmatchlimit` (matching brackets, `/*` comment
//! and raw-string starts), and the start-brace, matching-paren and last-paren searches.

use super::lex::{
    at, char_len, check_linecomment, cin_islinecomment, head_off, is_pos_in_string, last_byte,
    skip_string, skipwhite, starts, strend, MARK_COMMENT, MARK_RAW,
};
use super::{Ci, Pos};
use crate::findmatch::scan::{Backslashes, MatchScan, MatchStep};

/// The number of backslashes right before `linep[col]` (nvim's `check_prevcol` loop, stepping back
/// with `utf_head_off`).
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

/// The number of backslash bytes right before `linep[col]` (nvim's loop before a `"`).
fn backslash_bytes_before(linep: &[u8], col: usize) -> usize {
    linep[..col]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
}

/// The C indent counts as nvim does: raw bytes before a `"`, `utf_head_off` steps before a bracket.
const CINDENT_BACKSLASHES: Backslashes = Backslashes {
    quote: backslash_bytes_before,
    escape: backslashes_before,
};

/// The scan position as the C indent's [`Pos`] (1-based line).
fn to_pos((lnum, col): (usize, usize)) -> Pos {
    Pos { lnum, col }
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
        if s.backwards && s.comment_dir != 0 {
            s.comment_col = check_linecomment(s.linep);
        }
        s.run(|s| self.fml_step(s, blockstop, maxtravel))
            .map(to_pos)
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
        let mut comment_dir = 0; // only ever backward (-1) here
        let mut match_escaped = 0usize;
        let pairs: [(u8, u8); 3] = [(b'(', b')'), (b'{', b'}'), (b'[', b']')];
        match initc {
            b'*' | b'R' => {
                comment_dir = -1;
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
        let mut s = MatchScan::new((pos.lnum, pos.col), linep, CINDENT_BACKSLASHES);
        s.initc = initc;
        s.findc = findc;
        s.backwards = backwards;
        s.raw_string = raw_string;
        s.comment_dir = comment_dir;
        s.match_escaped = match_escaped;
        Some(s)
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
        if s.comment_dir != 0 {
            return self.fml_comment(s);
        }
        let lnum = s.pos.0;
        s.count_quotes(strend(s.linep, 0), || {
            lnum > 1 && last_byte(self.line(lnum - 1)) == b'\\'
        });
        s.match_char()
    }

    /// `findmatchlimit`, "Go to the next position, forward or backward": the previous or next char,
    /// crossing to the next line at a line end, until the buffer edge or `maxtravel` lines.
    fn fml_advance(&self, s: &mut MatchScan<'a>, maxtravel: i64) -> MatchStep {
        if s.backwards {
            if s.pos.1 == 0 {
                if s.pos.0 <= 1 {
                    return MatchStep::Break;
                }
                s.pos.0 -= 1;
                if maxtravel > 0 {
                    s.traveled += 1;
                    if s.traveled > maxtravel {
                        return MatchStep::Break;
                    }
                }
                s.linep = self.line(s.pos.0);
                s.do_quotes = -1;
                if s.comment_dir != 0 {
                    // The comment and raw-string searches only stop on their marks, which never
                    // cross a line end: a line without one is passed over at once (as the scan
                    // would, char by char).
                    let bit = if s.raw_string { MARK_RAW } else { MARK_COMMENT };
                    if !self.has_mark(s.pos.0, bit) {
                        s.pos.1 = 0;
                        return MatchStep::Next;
                    }
                    s.comment_col = check_linecomment(s.linep);
                }
                s.pos.1 = strend(s.linep, 0);
            } else {
                s.pos.1 -= 1;
                s.pos.1 -= head_off(s.linep, s.pos.1);
            }
        } else if at(s.linep, s.pos.1) == 0 {
            if s.pos.0 >= self.count() {
                return MatchStep::Break;
            }
            s.pos.0 += 1;
            if maxtravel != 0 {
                let t = s.traveled;
                s.traveled += 1;
                if t > maxtravel {
                    return MatchStep::Break;
                }
            }
            s.linep = self.line(s.pos.0);
            s.pos.1 = 0;
            s.do_quotes = -1;
        } else {
            s.pos.1 += char_len(s.linep, s.pos.1);
        }
        MatchStep::Go
    }

    /// `findmatchlimit`, the `comment_dir` branch: "Note: comments do not nest, and we ignore quotes
    /// in them", with "Possible start of raw string" for `R`; for `*` the shared backward `/*`
    /// search ([`MatchScan::comment_backward`]).
    fn fml_comment(&self, s: &mut MatchScan<'a>) -> MatchStep {
        if !s.raw_string {
            return s.comment_backward();
        }
        let linep = s.linep;
        let pos = to_pos(s.pos);
        if pos.col == 0 {
            return MatchStep::Next;
        }
        if at(linep, pos.col - 1) == b'R'
            && at(linep, pos.col) == b'"'
            && linep[pos.col + 1..strend(linep, 0)].contains(&b'(')
        {
            let end = if s.count > 0 {
                to_pos(s.match_pos)
            } else {
                self.cur
            };
            if !self.find_rawstring_end(linep, pos, end) {
                s.count += 1;
                s.match_pos = (pos.lnum, pos.col - 1);
            }
        }
        MatchStep::Next
    }
}

/// `findmatchlimit`, "If FM_BLOCKSTOP given, stop at a '{' or '}' in column 0": the match when it is
/// that brace, else the end of the search.
fn fml_blockstop(s: &MatchScan<'_>, blockstop: bool) -> MatchStep {
    let c0 = at(s.linep, 0);
    if s.pos.1 == 0 && blockstop && (c0 == b'{' || c0 == b'}') {
        if c0 == s.findc && s.count == 0 {
            return MatchStep::Return(Some(s.pos));
        }
        return MatchStep::Break;
    }
    MatchStep::Go
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
