//! The Vim-pattern lexer: classifies each token as an operator or a literal under the current magic
//! level (Neovim's `peekchr`; see the `pattern` module docs).

use super::Magic;

/// Characters that a backslash toggles between literal and operator (Vim's `META`). `^` and `$` are not
/// here: `\^` / `\$` are operators only at `\V`. Anything else after a backslash is a literal.
const META: &str = "%&()*+.123456789<=>?@ACDFHIKLMOPSUVWXZ[_acdfhiklmnopsuvwxz{|~";
/// Characters that are always special after a backslash inside `[]` (Vim's `REGEXP_INRANGE`).
pub(super) const IN_RANGE: &str = "]^-n\\";
/// Backslash abbreviations (Vim's `REGEXP_ABBR`); outside `[]` only `\r \t \e \b` reach this table, the
/// rest are `META`.
pub(super) const ABBR: &str = "nrtebdoxuU";
/// The class atoms `\i \I \k … \U` plus `.` (Vim's `classchars`).
pub(super) const CLASS_CHARS: &str = ".iIkKfFpPsSdDxXoOwWhHaAlLuU";

/// One lexed token: an operator (`Magic(c)` in Vim's source), a literal character, or end of pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tok {
    Op(char),
    Lit(char),
    End,
}

impl Tok {
    /// The character with its magicness dropped (Vim's `no_Magic`); `None` at the end.
    pub(super) fn ch(self) -> Option<char> {
        match self {
            Tok::Op(c) | Tok::Lit(c) => Some(c),
            Tok::End => None,
        }
    }
}

/// Is `t` a multi (`*` `+` `=` `?` `{` `@`)? Vim's `re_multi_type`.
pub(super) fn is_multi(t: Tok) -> bool {
    matches!(t, Tok::Op('*' | '+' | '=' | '?' | '{' | '@'))
}

/// `\r \t \e \b` → their control characters (Vim's `backslash_trans`).
pub(super) fn backslash_trans(c: char) -> char {
    match c {
        'r' => '\r',
        't' => '\t',
        'e' => '\x1b',
        'b' => '\x08',
        other => other,
    }
}

/// The lexer: Vim's `peekchr` / `skipchr` with the same position state, so that `^`, `$` and `*` are
/// classified exactly as Vim classifies them.
pub(super) struct Lexer<'a> {
    pub(super) s: &'a [char],
    pub(super) pos: usize,
    pub(super) magic: Magic,
    /// On the first token of a branch (Vim's `at_start`).
    at_start: bool,
    /// The previous token was the first of a branch (Vim's `prev_at_start`).
    prev_at_start: bool,
    pub(super) prev: Option<Tok>,
    prevprev: Option<Tok>,
    /// The peeked token and its length in chars (Vim's `curchr`).
    cur: Option<(Tok, usize)>,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(s: &'a [char], magic: Magic) -> Self {
        Lexer {
            s,
            pos: 0,
            magic,
            at_start: true,
            prev_at_start: false,
            prev: None,
            prevprev: None,
            cur: None,
        }
    }

    fn at_least_magic(&self) -> bool {
        matches!(self.magic, Magic::Very | Magic::Magic)
    }

    pub(super) fn peek(&mut self) -> Tok {
        if let Some((t, _)) = self.cur {
            return t;
        }
        let tl = self.classify();
        self.cur = Some(tl);
        tl.0
    }

    pub(super) fn skip(&mut self) {
        let (t, len) = match self.cur.take() {
            Some(tl) => tl,
            None => self.classify(),
        };
        self.pos += len;
        self.prev_at_start = self.at_start;
        self.at_start = false;
        self.prevprev = self.prev;
        self.prev = Some(t);
    }

    pub(super) fn get(&mut self) -> Tok {
        let t = self.peek();
        self.skip();
        t
    }

    /// Skip a token that is transparent to position state (`\c` `\C` `\v` `\m` `\M` `\V`): Vim's
    /// `skipchr_keepstart`.
    pub(super) fn skip_keepstart(&mut self) {
        let (was, prev, prevprev) = (self.prev_at_start, self.prev, self.prevprev);
        self.skip();
        self.at_start = was;
        self.prev = prev;
        self.prevprev = prevprev;
    }

    /// Reposition after reading raw characters (collections, `\%d123`, `\{n,m}`); the token cache is
    /// invalid (Vim sets `curchr = -1`).
    pub(super) fn seek(&mut self, pos: usize) {
        self.pos = pos;
        self.cur = None;
    }

    /// Classify the token at `pos` (Vim's `peekchr` body).
    fn classify(&mut self) -> (Tok, usize) {
        let Some(&c) = self.s.get(self.pos) else {
            return (Tok::End, 0);
        };
        if c != '\\' {
            return (self.bare(c, self.pos, false), 1);
        }
        let Some(&n) = self.s.get(self.pos + 1) else {
            return (Tok::Lit('\\'), 1); // a trailing backslash is a literal backslash
        };
        if META.contains(n) {
            // Fetch the next character as if bare and toggle its magicness.
            self.prev_at_start = self.at_start;
            self.at_start = false; // so that "/\*ptr" works
            let t = match self.bare(n, self.pos + 1, true) {
                Tok::Op(x) => Tok::Lit(x),
                Tok::Lit(x) => Tok::Op(x),
                Tok::End => Tok::End,
            };
            (t, 2)
        } else if ABBR.contains(n) {
            (Tok::Lit(backslash_trans(n)), 2)
        } else if self.magic == Magic::VeryNoMagic && (n == '^' || n == '$') {
            (Tok::Op(n), 2)
        } else {
            (Tok::Lit(n), 2) // never magic, so the backslash changes nothing
        }
    }

    /// Classify a bare character `c` at index `at` under the current level and position state.
    fn bare(&mut self, c: char, at: usize, after_slash: bool) -> Tok {
        let op = match c {
            '.' | '[' | '~' => self.at_least_magic(),
            '(' | ')' | '{' | '%' | '+' | '=' | '?' | '@' | '!' | '&' | '|' | '<' | '>' | '#'
            | '"' | '\'' | ',' | '-' | ':' | ';' | '`' | '/' => self.magic == Magic::Very,
            // `*` is literal as a branch's first item, right after a leading `^`, and right after
            // `\(` `\|` `\&` (unless it is itself backslashed: "\(\*" toggles a magic star).
            '*' => {
                self.at_least_magic()
                    && !self.at_start
                    && !(self.prev_at_start && self.prev == Some(Tok::Op('^')))
                    && (after_slash || !matches!(self.prev, Some(Tok::Op('(' | '&' | '|'))))
            }
            // `^` is an anchor only at a branch start (or anywhere at `\v`).
            '^' => {
                let op = self.magic != Magic::VeryNoMagic
                    && (self.at_start
                        || self.magic == Magic::Very
                        || matches!(self.prev, Some(Tok::Op('(' | '|' | '&' | 'n')))
                        || (self.prev.and_then(Tok::ch) == Some('(')
                            && self.prevprev == Some(Tok::Op('%'))));
                if op {
                    self.at_start = true;
                    self.prev_at_start = false;
                }
                op
            }
            // `$` is an anchor only before a branch end (or anywhere at `\v`).
            '$' => self.magic != Magic::VeryNoMagic && self.dollar_ends_branch(at),
            _ => false,
        };
        if op {
            Tok::Op(c)
        } else {
            Tok::Lit(c)
        }
    }

    /// Vim's look-ahead for a magic `$`: end of pattern or `\|` `\&` `\)` `\n` next, skipping any
    /// `\c \C \m \M \v \V \Z` in between.
    fn dollar_ends_branch(&self, at: usize) -> bool {
        let s = self.s;
        let mut p = at + 1;
        let mut very = self.magic == Magic::Very;
        while s.get(p) == Some(&'\\')
            && matches!(s.get(p + 1), Some('c' | 'C' | 'm' | 'M' | 'v' | 'V' | 'Z'))
        {
            match s[p + 1] {
                'v' => very = true,
                'm' | 'M' | 'V' => very = false,
                _ => {}
            }
            p += 2;
        }
        p >= s.len()
            || (s[p] == '\\' && matches!(s.get(p + 1), Some('|' | '&' | ')' | 'n')))
            || (very && matches!(s[p], '|' | '&' | ')'))
            || self.magic == Magic::Very
    }
}
