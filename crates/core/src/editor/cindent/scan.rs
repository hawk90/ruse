//! The searches over lines the C indent runs: `findmatchlimit` (matching brackets, `/*` comment
//! and raw-string starts), and the start-brace, matching-paren and last-paren searches.

use super::lex::{
    at, char_len, check_linecomment, cin_islinecomment, head_off, is_pos_in_string, last_byte,
    skip_string, skipwhite, starts, strend, MARK_COMMENT, MARK_RAW,
};
use super::{Ci, Pos};

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// `findmatchlimit(NULL, initc, flags, maxtravel)` from the cursor. `initc` is `*` (a `/*` comment
    /// start, backward), `R` (a raw string start, backward), a bracket (its match: `{`, `(`, `[` look
    /// backward for the opener, `)` forward for the closer), or 0 (the bracket under or after the
    /// cursor). `blockstop`: stop at a `{` or `}` in column 0.
    pub(super) fn findmatchlimit(&self, initc: u8, blockstop: bool, maxtravel: i64) -> Option<Pos> {
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
