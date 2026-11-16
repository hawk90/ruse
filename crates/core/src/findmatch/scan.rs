//! The scan loop of nvim v0.12.6's `findmatchlimit` (`search.c`) that both of ruse's ports run: `%`
//! ([`crate::findmatch`]) and the C indent's (`crate::editor::cindent::scan`). Each port chooses its
//! target and steps from position to position itself; from the position on, the backward `/*` comment
//! search, the quote count and the `switch (c = ...)` on the char are this one implementation.
//!
//! What the ports set differently on the one state ([`MatchScan`]):
//!
//! - the line numbers in `pos` and `match_pos`: 0-based under `%`, 1-based (nvim's) in the C indent;
//!   only the port's own stepping and its `prev_ends_in_backslash` read them;
//! - `comment_dir`: `%` searches both ways (`1`, `-1`), the C indent only backward (`-1`), and only
//!   the C indent searches for a raw string start (`raw_string`, its own phase);
//! - `lisp`: `'lisp'` under `%`, never in the C indent;
//! - [`Backslashes`]: how the backslashes before a char are counted, see there;
//! - `traveled` (the C indent's `maxtravel`) and `lispcomm` (`%` under `'lisp'`) are read only by the
//!   port's own stepping.

/// The byte at `i` of `s`, NUL past the end.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `check_prevcol` as `%` runs it: whether the character before `line[col]` is `ch`, `prevcol` taking
/// its column. It steps back over every continuation byte, where nvim's `utf_head_off` stops at three
/// and at one no valid char owns: the two only part on bytes that are not UTF-8.
pub(crate) fn check_prevcol(line: &[u8], col: usize, ch: u8, prevcol: &mut usize) -> bool {
    if col == 0 {
        return false;
    }
    let mut c = col - 1;
    while c > 0 && line[c] & 0xC0 == 0x80 {
        c -= 1;
    }
    *prevcol = c;
    line[c] == ch
}

/// How a port counts the backslashes right before `linep[col]`. nvim counts raw bytes before a `"`
/// and steps back with `check_prevcol` (`utf_head_off`) before a bracket; the C indent port does
/// exactly that, `%` uses its own [`check_prevcol`] for both. On UTF-8 text the three counts agree
/// (a backslash is never a continuation byte); they can part only on bytes that are not UTF-8, so
/// each port keeps its own.
#[derive(Clone, Copy)]
pub(crate) struct Backslashes {
    /// Before a `"` (whether it flips `inquote`).
    pub(crate) quote: fn(&[u8], usize) -> usize,
    /// Before `initc` / `findc` and the starting bracket (`match_escaped`).
    pub(crate) escape: fn(&[u8], usize) -> usize,
}

/// The state the `findmatchlimit` scan loop carries from one position to the next.
pub(crate) struct MatchScan<'a> {
    /// The current search position (line, column; nvim's static `pos`).
    pub(crate) pos: (usize, usize),
    /// The text of line `pos.0`.
    pub(crate) linep: &'a [u8],
    /// The char whose match is wanted (counted up), 0 for a comment or raw-string search.
    pub(crate) initc: u8,
    /// The matching char (counted down), 0 for a comment or raw-string search.
    pub(crate) findc: u8,
    pub(crate) backwards: bool,
    /// 1 forward, -1 backward, 0 not a comment or raw-string search.
    pub(crate) comment_dir: i32,
    /// Searching for a raw string start (`initc` `R`).
    pub(crate) raw_string: bool,
    /// 1 when the match wanted is escaped (an odd number of backslashes before it).
    pub(crate) match_escaped: usize,
    /// The cumulative number of unmatched `initc`s (or comment and raw-string starts).
    pub(crate) count: i64,
    /// Where the last comment or raw-string start was found.
    pub(crate) match_pos: (usize, usize),
    /// The start of the line's `//` (or, under `'lisp'`, `;`) comment, if any.
    pub(crate) comment_col: Option<usize>,
    /// The parity of the quotes on the line, -1 until counted.
    pub(crate) do_quotes: i32,
    /// Whether the search started inside quotes, `None` until known.
    pub(crate) start_in_quotes: Option<bool>,
    /// True when inside quotes.
    pub(crate) inquote: bool,
    /// How many lines the search has gone so far.
    pub(crate) traveled: i64,
    /// `'lisp'`: `#\(` and the like are characters, not brackets.
    pub(crate) lisp: bool,
    /// Inside a `'lisp'` comment: the match is searched in it only.
    pub(crate) lispcomm: bool,
    pub(crate) bslcnt: Backslashes,
}

/// What a phase of one scan-loop iteration decides.
pub(crate) enum MatchStep {
    /// Go on to the next phase of this iteration.
    Go,
    /// The iteration is over: go to the next position (nvim's `continue`).
    Next,
    /// Leave the loop (nvim's `break`).
    Break,
    /// Return this result from `findmatchlimit` at once.
    Return(Option<(usize, usize)>),
}

impl<'a> MatchScan<'a> {
    /// The scan state at `pos` on `linep`, before the target is set: nothing counted, no quotes.
    pub(crate) fn new(pos: (usize, usize), linep: &'a [u8], bslcnt: Backslashes) -> Self {
        MatchScan {
            pos,
            linep,
            initc: 0,
            findc: 0,
            backwards: false,
            comment_dir: 0,
            raw_string: false,
            match_escaped: 0,
            count: 0,
            match_pos: (0, 0),
            comment_col: None,
            do_quotes: -1,
            start_in_quotes: None,
            inquote: false,
            traveled: 0,
            lisp: false,
            lispcomm: false,
            bslcnt,
        }
    }

    /// The `findmatchlimit` scan loop (`while (!got_int)`): `iteration` runs one position's phases
    /// until one breaks or returns; a backward comment or raw-string search that ran out returns the
    /// last start it found.
    pub(crate) fn run(
        mut self,
        mut iteration: impl FnMut(&mut Self) -> MatchStep,
    ) -> Option<(usize, usize)> {
        loop {
            match iteration(&mut self) {
                MatchStep::Go | MatchStep::Next => {}
                MatchStep::Break => break,
                MatchStep::Return(r) => return r,
            }
        }
        if self.comment_dir < 0 && self.count > 0 {
            return Some(self.match_pos);
        }
        None
    }

    /// `findmatchlimit`, the backward comment search ("A comment may contain / * or / /, it may also
    /// start or end with / * /. Ignore a / * after / / and after *."): a `/*` is counted, a `*/`
    /// returns the start found (or the `/*` it closes); anything else goes to the next position.
    pub(crate) fn comment_backward(&mut self) -> MatchStep {
        let (linep, pos) = (self.linep, self.pos);
        if pos.1 == 0 {
            return MatchStep::Next;
        }
        if at(linep, pos.1 - 1) == b'/'
            && at(linep, pos.1) == b'*'
            && (pos.1 == 1 || at(linep, pos.1 - 2) != b'*')
            && self.comment_col.is_none_or(|cc| pos.1 < cc)
        {
            self.count += 1;
            self.match_pos = (pos.0, pos.1 - 1);
        } else if at(linep, pos.1 - 1) == b'*' && at(linep, pos.1) == b'/' {
            if self.count > 0 {
                return MatchStep::Return(Some(self.match_pos));
            } else if pos.1 > 1
                && at(linep, pos.1 - 2) == b'/'
                && self.comment_col.is_none_or(|cc| pos.1 <= cc)
            {
                return MatchStep::Return(Some((pos.0, pos.1 - 2)));
            }
            return MatchStep::Return(None);
        }
        MatchStep::Next
    }

    /// `findmatchlimit`, "If smart matching ('cpoptions' does not contain '%'), braces inside of
    /// quotes are ignored, but only if there is an even number of quotes in the line": on a new line
    /// (`do_quotes == -1`), the parity of the quotes before `end` (where the port's line ends), and "If
    /// we find an uneven count, check current line and previous one for a '\' at the end"
    /// (`prev_ends_in_backslash`: the line before `pos.0` exists and ends in one).
    pub(crate) fn count_quotes(
        &mut self,
        end: usize,
        prev_ends_in_backslash: impl FnOnce() -> bool,
    ) {
        if self.do_quotes == -1 {
            let (linep, pos, backwards) = (self.linep, self.pos, self.backwards);
            let mut at_start = self.do_quotes;
            let mut i = 0;
            while i < end {
                if i == pos.1 + usize::from(backwards) {
                    at_start = self.do_quotes & 1;
                }
                if linep[i] == b'"'
                    && (i == 0 || linep[i - 1] != b'\'' || at(linep, i + 1) != b'\'')
                {
                    self.do_quotes += 1;
                }
                if linep[i] == b'\\' && i + 1 < end {
                    i += 1;
                }
                i += 1;
            }
            self.do_quotes &= 1;
            if self.do_quotes == 0 {
                self.inquote = false;
                if end > 0 && linep[end - 1] == b'\\' {
                    self.do_quotes = 1;
                    if self.start_in_quotes.is_none() {
                        self.inquote = true;
                        self.start_in_quotes = Some(true);
                    } else if backwards {
                        self.inquote = true;
                    }
                }
                if prev_ends_in_backslash() {
                    self.do_quotes = 1;
                    if self.start_in_quotes.is_none() {
                        self.inquote = at_start != 0;
                        if self.inquote {
                            self.start_in_quotes = Some(true);
                        }
                    } else if !backwards {
                        self.inquote = true;
                    }
                }
            }
        }
        if self.start_in_quotes.is_none() {
            self.start_in_quotes = Some(false);
        }
    }

    /// `findmatchlimit`, the `switch (c = ...)` on the char at `pos`: "If 'smartmatch' is set: Things
    /// inside quotes are ignored by setting 'inquote'": a line end without a trailing `\` resets it,
    /// an unescaped `"` flips it; any other char is skipped as a char literal or `'lisp'` `#\(`, else
    /// counted ([`Self::count_match`]).
    pub(crate) fn match_char(&mut self) -> MatchStep {
        let (linep, pos) = (self.linep, self.pos);
        let c = at(linep, pos.1);
        match c {
            0 => {
                if pos.1 == 0 || linep[pos.1 - 1] != b'\\' {
                    self.inquote = false;
                    self.start_in_quotes = Some(false);
                }
            }
            b'"' => {
                if self.do_quotes != 0 && (self.bslcnt.quote)(linep, pos.1).is_multiple_of(2) {
                    self.inquote = !self.inquote;
                    self.start_in_quotes = Some(false);
                }
            }
            _ => {
                if self.skip_char_literal(c) || self.lisp_char(c) {
                    return MatchStep::Next;
                }
                return self.count_match(c);
            }
        }
        MatchStep::Next
    }

    /// `findmatchlimit`, "Skip things in single quotes: 'x' or '\x'": whether `c` opens (or, backward,
    /// closes) one; the scan then stands on its other quote.
    fn skip_char_literal(&mut self, c: u8) -> bool {
        if c != b'\'' || self.initc == b'\'' || self.findc == b'\'' {
            return false;
        }
        let linep = self.linep;
        let col = &mut self.pos.1;
        if self.backwards {
            if *col > 1 {
                if linep[*col - 2] == b'\'' {
                    *col -= 2;
                    return true;
                } else if linep[*col - 2] == b'\\' && *col > 2 && linep[*col - 3] == b'\'' {
                    *col -= 3;
                    return true;
                }
            }
        } else if at(linep, *col + 1) != 0 {
            if at(linep, *col + 1) == b'\\'
                && at(linep, *col + 2) != 0
                && at(linep, *col + 3) == b'\''
            {
                *col += 3;
                return true;
            } else if at(linep, *col + 2) == b'\'' {
                *col += 2;
                return true;
            }
        }
        false
    }

    /// `findmatchlimit`, "For Lisp skip over backslashed (), {} and []. (actually, we skip #\( et
    /// al)": whether `c` is such a character under `'lisp'`.
    fn lisp_char(&self, c: u8) -> bool {
        let (linep, col) = (self.linep, self.pos.1);
        let mut p = 0;
        self.lisp
            && b"(){}[]".contains(&c)
            && col > 1
            && check_prevcol(linep, col, b'\\', &mut p)
            && check_prevcol(linep, col - 1, b'#', &mut p)
    }

    /// `findmatchlimit`, "Check for match outside of quotes, and inside of quotes when the start is
    /// also inside of quotes": `c` counts up as `initc`, down as `findc`, and at zero is the match;
    /// "Only accept a match when ... escaping is what we expect".
    fn count_match(&mut self, c: u8) -> MatchStep {
        if (!self.inquote || self.start_in_quotes == Some(true))
            && (c == self.initc || c == self.findc)
            && (self.bslcnt.escape)(self.linep, self.pos.1) & 1 == self.match_escaped
        {
            if c == self.initc {
                self.count += 1;
            } else {
                if self.count == 0 {
                    return MatchStep::Return(Some(self.pos));
                }
                self.count -= 1;
            }
        }
        MatchStep::Next
    }
}
