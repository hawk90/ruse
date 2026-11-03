//! C-REGEX: the Vim-dialect regex abstraction (D-028, `docs/design/vim-regex.md`).
//!
//! A magic-aware FRONT-END parses a Vim search pattern into Rust-`regex` syntax, resolving the magic
//! levels (`\v` very-magic / `\m` magic / `\M` nomagic / `\V` very-nomagic) exactly once and lowering
//! `\zs`/`\ze` match-boundary overrides onto reserved empty capture groups (design §4). The wrapped Rust
//! `regex` crate is the SOLE engine for MVP (DEP-REGEX: linear-time, ReDoS-immune); its types never
//! reach this module's public API (INV-CONTRACT-FIRST). Atoms the crate cannot represent — lookaround
//! (`\@=` `\@<=` …), backrefs (`\1`), `\&`, `~`, position atoms (`\%V` `\%23l` `\%^` …), `\Z`,
//! equivalence classes `[=a=]` — are **rejected with a typed error, never faked** (design §2). The owned
//! NFA engine that would support them is post-MVP (scope decided 2026-08-11: MVP ships the wrapped
//! engine alone).
//!
//! What "magic" means, and why it is not a vocabulary difference: in the DEFAULT level `\m`, the bytes
//! `+ ? { } ( ) | < >` are LITERAL and must be backslash-escaped to become operators — the opposite of
//! Rust/PCRE, where they are operators bare. The front-end owns that inversion so the two never smear.
//!
//! How the front-end works (issue #692). It is a port of Neovim's own pattern reader (`regexp.c`:
//! `peekchr`, `nfa_regatom`, `nfa_regpiece`, the collection reader), not a char-by-char rewrite:
//!
//! - A **lexer** classifies each input token, given the current magic level and the position state
//!   (at branch start, previous token), as an operator (`Tok::Op`) or a literal (`Tok::Lit`). A
//!   backslash toggles the magicness of the characters Vim lists as "may be magic" (`META`), so `\(`
//!   is an operator at `\m` and a literal at `\v`. `^` is an operator only at a branch start (or after
//!   `\(` `\|` `\&` `\n`), `$` only before a branch end, `*` is literal at a branch start.
//! - A recursive-descent **parser** turns operators into Rust syntax (groups, multis, classes,
//!   anchors) or into a typed error. Collections `[...]` are read raw (they have their own grammar:
//!   ranges, `[:name:]`, `\e \t \r \b \n \d123 \o40 \x20 €`, a backslash before anything else is
//!   a literal backslash).
//! - **Unknown atoms are errors, never letters.** A backslash before a character Vim never makes
//!   magic (`\q`, `\-`, `\/`) IS a literal in Vim, and stays one here; a backslash before one Vim
//!   gives meaning to is either honoured or rejected.
//!
//! The ground truth for every behaviour here is the differential table in
//! `crates/core/tests/pattern_oracle.rs`, captured from the pinned Neovim by
//! `tools/parity/pattern_oracle.py`.

use regex::Regex as RawRegex;

/// The magic level a pattern starts in (it may switch mid-pattern with `\v`/`\V`/`\m`/`\M`). `Magic` is
/// Vim's default and matches the `'magic'` option being on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Magic {
    /// `\v` very-magic: most punctuation is an operator without a backslash (closest to Rust/PCRE).
    Very,
    /// `\m` magic (Vim default): `. * [ ] ^ $` are operators bare; `+ ? { } ( ) | < >` need a backslash.
    #[default]
    Magic,
    /// `\M` nomagic: only `^ $` are operators bare; `.` and `*` need a backslash.
    NoMagic,
    /// `\V` very-nomagic: only `\` is special — every other character is literal.
    VeryNoMagic,
}

/// How to compile a pattern (`'ignorecase'` / `'smartcase'`; the starting magic level).
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The magic level the pattern starts in.
    pub magic: Magic,
    /// `'ignorecase'`: match case-insensitively.
    pub ignore_case: bool,
    /// `'smartcase'`: when `ignore_case` is on, an UPPERCASE letter anywhere in the pattern forces a
    /// case-SENSITIVE match (Vim's case-smart search). No effect when `ignore_case` is off.
    pub smart_case: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            magic: Magic::Magic,
            ignore_case: false,
            smart_case: false,
        }
    }
}

/// Why a Vim pattern could not be compiled. Typed, not stringly (D-041); an unrepresentable atom is a
/// distinct outcome from a genuinely malformed pattern so the command layer can message each honestly.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegexError {
    /// An atom the MVP (wrapped-`regex`) engine cannot represent — lookaround, backrefs, `\&`, `~`,
    /// position atoms, `\Z`, equivalence classes. Rejected, never approximated, so a real Vim pattern
    /// never silently matches the wrong span.
    Unsupported(&'static str),
    /// The pattern is malformed (unbalanced group, bad `\{…}`, misplaced multi, reversed range, …) or the
    /// Rust engine rejected the lowered form. Carries a message for the status line.
    Syntax(String),
}

/// One match, as byte offsets `[start, end)` into the haystack — with any `\zs`/`\ze` boundary override
/// already applied, so `start`/`end` are the REPORTED span a caller highlights or replaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Match {
    /// Byte offset of the (possibly `\zs`-adjusted) match start.
    pub start: usize,
    /// Byte offset of the (possibly `\ze`-adjusted) match end.
    pub end: usize,
}

/// A compiled Vim-dialect pattern (C-REGEX). Holds the wrapped engine and the empty capture groups that
/// mark each `\zs` / `\ze` in the pattern.
#[derive(Clone, Debug)]
pub struct Regex {
    inner: RawRegex,
    /// Capture groups (each an empty `()`) standing where a `\zs` was written. The reported start is the
    /// rightmost one that participated — "the last one on the matching path wins" (design §4), since a
    /// path only moves forward through the text.
    zs: Vec<usize>,
    /// Same for `\ze`: the reported end.
    ze: Vec<usize>,
    /// The pattern has an item that matches a line break (`\n`, `\_x`, `\_[…]`, `[\n]`): Vim's
    /// `re_multiline`. Only such a pattern can match across lines, so `:s` and `:g` search the whole
    /// buffer for it instead of one line at a time.
    multiline: bool,
}

impl Regex {
    /// Compile a Vim `pattern` under `opts`. Lowers the magic levels, case flags and `\zs`/`\ze` to Rust
    /// syntax, then builds the wrapped engine.
    ///
    /// # Errors
    /// [`RegexError::Unsupported`] for an atom the wrapped engine cannot represent;
    /// [`RegexError::Syntax`] for a malformed pattern or one the engine rejects.
    pub fn compile(pattern: &str, opts: Options) -> Result<Regex, RegexError> {
        if pattern.is_empty() {
            return Err(RegexError::Syntax("empty pattern".into()));
        }
        // 'smartcase': an uppercase letter in the pattern forces case-sensitivity — counted the way Vim's
        // `pat_has_uppercase` counts it (an escaped `\S` is an atom, not an uppercase letter).
        let base = opts.ignore_case && !(opts.smart_case && pat_has_uppercase(pattern, opts.magic));
        let mut low = lower(pattern, opts.magic, base)?;
        // `\c` / `\C` anywhere in the pattern override both options (`\c` wins if both appear). The
        // collection emitter needs the final answer, so re-lower once if a flag changed it.
        let case_insensitive = low.case.unwrap_or(base);
        if case_insensitive != base {
            low = lower(pattern, opts.magic, case_insensitive)?;
        }
        // No `(?i)`: under 'ignorecase' the lowering already spelled each literal as its nvim fold class
        // (#1112), since the engine's own Unicode folding is not Vim's (`ß` / `ẞ`, the iota-subscript
        // letters, letters newer than the engine's tables).
        let inner = regex::RegexBuilder::new(&low.rust)
            .build()
            .map_err(|e| RegexError::Syntax(e.to_string()))?;
        Ok(Regex {
            inner,
            zs: low.zs,
            ze: low.ze,
            multiline: low.has_nl,
        })
    }

    /// Whether the pattern can match a line break (Vim's `re_multiline`), so a match may span lines.
    #[must_use]
    pub fn multiline(&self) -> bool {
        self.multiline
    }

    /// Like [`Regex::find_at`], but also returns where the WHOLE match starts, before any `\zs` moves
    /// the reported start. Vim's `:s` and `:g` take a match as belonging to the line its whole match
    /// starts on, even when a `\zs` after a `\n` reports a start on a later line.
    #[must_use]
    pub fn find_whole_at(&self, hay: &str, from: usize) -> Option<(usize, Match)> {
        let from = from.min(hay.len());
        let caps = self.inner.captures_at(hay, from)?;
        if phantom_line(hay, &caps) {
            return None;
        }
        let whole = caps.get(0).expect("group 0 always present").start();
        Some((whole, self.report(&caps)))
    }

    /// The first match at a byte offset `>= from`, honoring the `\zs`/`\ze` reported span. `None` if
    /// there is no match from `from` to the end (the caller wraps for `/`-search).
    #[must_use]
    pub fn find_at(&self, hay: &str, from: usize) -> Option<Match> {
        let from = from.min(hay.len());
        let caps = self.inner.captures_at(hay, from)?;
        if phantom_line(hay, &caps) {
            return None; // the only thing left to match is the slot past the last line
        }
        Some(self.report(&caps))
    }

    /// Every match `:s///g` replaces in each line of `hay`, left to right (also `hlsearch`). This is the
    /// loop of nvim's `do_sub` (v0.12.6), run per line:
    ///
    /// - the next match is searched from the REPORTED end of the last one, so a `\ze` lets the next
    ///   match overlap the rest of the whole match (`:s/\v(ab)\zeab/-/g` on "ababab" gives "--ab");
    /// - an empty match right where the previous match ended does not count: the scan steps one
    ///   character instead (`x*` on "abc" is empty before each character, `a*` on "aax" is "aa" only);
    /// - for a pattern that cannot match a line break, the scan of a line stops once it reaches the end
    ///   of the line, so an empty match there counts only when the scan got there by a match, not by
    ///   stepping: `:s/x*/-/g` on "abc" gives "-a-b-c", `:s/\>/-/g` on "a b" gives "a- b-", and
    ///   `:s/a\|$/-/g` on "ab" gives "-b-".
    ///
    /// Each line is scanned on its own (a line's scan restarts the empty-match rule), and the scan never
    /// stalls: every step either moves past a match or steps one character.
    #[must_use]
    pub fn find_all(&self, hay: &str) -> Vec<Match> {
        self.scan_lines(hay, true)
    }

    /// Every match a `/` or `?` search can land on in each line of `hay`, left to right: nvim's
    /// `searchit` with the default `'cpoptions'` flag `c`. The next match is searched from the reported
    /// end of the last one, and an empty match steps one character, so unlike [`Regex::find_all`] every
    /// empty match is kept (`/\<` lands on each word start, `/$` on each line end) and a match that runs
    /// onto a later line ends its line's scan.
    #[must_use]
    pub fn search_all(&self, hay: &str) -> Vec<Match> {
        self.scan_lines(hay, false)
    }

    /// The per-line scan behind [`Regex::find_all`] (`substitute`) and [`Regex::search_all`].
    fn scan_lines(&self, hay: &str, substitute: bool) -> Vec<Match> {
        let mut out: Vec<Match> = Vec::new();
        let n = hay.len();
        let mut ls = 0;
        // The slot after a final "\n" is not a line (see `phantom_line`).
        while ls < n || (ls == n && !(n > 0 && hay.ends_with('\n'))) {
            let le = hay[ls..].find('\n').map_or(n, |i| ls + i);
            let mut next = le + 1;
            let mut col = ls;
            let mut prev_end: Option<usize> = None;
            loop {
                let Some(caps) = self.inner.captures_at(hay, col) else {
                    return out; // no match anywhere further down
                };
                if phantom_line(hay, &caps) {
                    return out;
                }
                let whole = caps.get(0).expect("group 0 always present").start();
                if whole > le {
                    // The leftmost match starts on a later line: no line before it has one.
                    next = hay[..whole].rfind('\n').map_or(0, |i| i + 1);
                    break;
                }
                let m = self.report(&caps);
                let step = |at: usize| at + hay[at..].chars().next().map_or(1, char::len_utf8);
                if substitute && prev_end == Some(col) && m.start == m.end && m.end == col {
                    // An empty match where the previous one ended does not count.
                    if col >= le {
                        break;
                    }
                    col = step(col);
                    if col >= le && !self.multiline {
                        break;
                    }
                    continue;
                }
                out.push(m);
                if m.end > le {
                    break; // it ran onto a later line
                }
                if substitute {
                    col = m.end;
                    prev_end = Some(col);
                    if col >= le && !self.multiline {
                        break;
                    }
                } else if m.start == m.end {
                    if m.end >= le {
                        break;
                    }
                    col = step(m.end);
                } else {
                    col = m.end;
                }
            }
            if next <= ls {
                break; // cannot happen (a match past `le` is on a later line); never loop
            }
            ls = next;
        }
        out
    }

    /// Resolve the reported span of a capture set: the rightmost participating `\zs` / `\ze` marker,
    /// else the whole match. A `\ze` left of the reported start collapses to an empty span at the start
    /// (nvim: `a\zeb\zsc` on `abc` reports `[2,2)`).
    fn report(&self, caps: &regex::Captures) -> Match {
        let whole = caps.get(0).expect("group 0 always present");
        let marker = |groups: &[usize]| {
            groups
                .iter()
                .filter_map(|&g| caps.get(g))
                .map(|m| m.start())
                .max()
        };
        let start = marker(&self.zs).unwrap_or(whole.start());
        let end = marker(&self.ze).unwrap_or(whole.end()).max(start);
        Match { start, end }
    }
}

/// Is this match the empty slot after a haystack's final `\n`? A whole-buffer haystack ends in "\n", and
/// the position after it is not a line in Vim: `^`, `$`, `\_$`, `x*` must not match there (nvim:
/// `/^$` on "abc\ndef\n" is E486, and `/^` wraps instead of landing past the end). A match that STARTS
/// there can only be zero-width, so dropping it never hides a real match. A haystack that does not end
/// in "\n" (one line, an empty buffer) is unaffected.
fn phantom_line(hay: &str, caps: &regex::Captures) -> bool {
    let whole = caps.get(0).expect("group 0 always present");
    !hay.is_empty() && whole.start() == hay.len() && hay.ends_with('\n')
}

/// Vim's `pat_has_uppercase` (search.c): does the pattern contain an uppercase letter for `'smartcase'`?
/// A backslash item (`\S`, `\_X`, `\%X`) is skipped at magic levels up to `\m`; at `\v` a `%X` / `_X`
/// pair is skipped but a backslash is not (the Vim quirk is kept). The level used is the pattern's last
/// `\v` / `\V` switch, as `skip_regexp_ex` reports it.
fn pat_has_uppercase(pattern: &str, magic0: Magic) -> bool {
    let s: Vec<char> = pattern.chars().collect();
    let mut level = magic0;
    let mut i = 0;
    while i < s.len() {
        let at_least_magic = matches!(level, Magic::Very | Magic::Magic);
        if (s[i] == '[' && at_least_magic)
            || (s[i] == '\\' && s.get(i + 1) == Some(&'[') && !at_least_magic)
        {
            let from = if s[i] == '[' { i + 1 } else { i + 2 };
            i = skip_anyof(&s, from);
            if i >= s.len() {
                break;
            }
        } else if s[i] == '\\' && i + 1 < s.len() {
            i += 1;
            match s[i] {
                'v' => level = Magic::Very,
                'V' => level = Magic::VeryNoMagic,
                _ => {}
            }
        }
        i += 1;
    }
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if !c.is_ascii() {
            if c.is_uppercase() {
                return true;
            }
            i += 1;
        } else if c == '\\' && level != Magic::Very {
            i += match s.get(i + 1) {
                Some('_' | '%') if i + 2 < s.len() => 3,
                Some(_) => 2,
                None => 1,
            };
        } else if (c == '%' || c == '_') && level == Magic::Very {
            i += if i + 1 < s.len() { 2 } else { 1 };
        } else if c.is_ascii_uppercase() {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------------
// Front-end: lexer + parser, ported from Neovim's regexp.c (see the module docs).
// ---------------------------------------------------------------------------------------------------

/// Characters that a backslash toggles between literal and operator (Vim's `META`). `^` and `$` are not
/// here: `\^` / `\$` are operators only at `\V`. Anything else after a backslash is a literal.
const META: &str = "%&()*+.123456789<=>?@ACDFHIKLMOPSUVWXZ[_acdfhiklmnopsuvwxz{|~";
/// Characters that are always special after a backslash inside `[]` (Vim's `REGEXP_INRANGE`).
const IN_RANGE: &str = "]^-n\\";
/// Backslash abbreviations (Vim's `REGEXP_ABBR`); outside `[]` only `\r \t \e \b` reach this table, the
/// rest are `META`.
const ABBR: &str = "nrtebdoxuU";
/// The class atoms `\i \I \k … \U` plus `.` (Vim's `classchars`).
const CLASS_CHARS: &str = ".iIkKfFpPsSdDxXoOwWhHaAlLuU";

/// One lexed token: an operator (`Magic(c)` in Vim's source), a literal character, or end of pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok {
    Op(char),
    Lit(char),
    End,
}

impl Tok {
    /// The character with its magicness dropped (Vim's `no_Magic`); `None` at the end.
    fn ch(self) -> Option<char> {
        match self {
            Tok::Op(c) | Tok::Lit(c) => Some(c),
            Tok::End => None,
        }
    }
}

/// Is `t` a multi (`*` `+` `=` `?` `{` `@`)? Vim's `re_multi_type`.
fn is_multi(t: Tok) -> bool {
    matches!(t, Tok::Op('*' | '+' | '=' | '?' | '{' | '@'))
}

/// `\r \t \e \b` → their control characters (Vim's `backslash_trans`).
fn backslash_trans(c: char) -> char {
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
struct Lexer<'a> {
    s: &'a [char],
    pos: usize,
    magic: Magic,
    /// On the first token of a branch (Vim's `at_start`).
    at_start: bool,
    /// The previous token was the first of a branch (Vim's `prev_at_start`).
    prev_at_start: bool,
    prev: Option<Tok>,
    prevprev: Option<Tok>,
    /// The peeked token and its length in chars (Vim's `curchr`).
    cur: Option<(Tok, usize)>,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a [char], magic: Magic) -> Self {
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

    fn peek(&mut self) -> Tok {
        if let Some((t, _)) = self.cur {
            return t;
        }
        let tl = self.classify();
        self.cur = Some(tl);
        tl.0
    }

    fn skip(&mut self) {
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

    fn get(&mut self) -> Tok {
        let t = self.peek();
        self.skip();
        t
    }

    /// Skip a token that is transparent to position state (`\c` `\C` `\v` `\m` `\M` `\V`): Vim's
    /// `skipchr_keepstart`.
    fn skip_keepstart(&mut self) {
        let (was, prev, prevprev) = (self.prev_at_start, self.prev, self.prevprev);
        self.skip();
        self.at_start = was;
        self.prev = prev;
        self.prevprev = prevprev;
    }

    /// Reposition after reading raw characters (collections, `\%d123`, `\{n,m}`); the token cache is
    /// invalid (Vim sets `curchr = -1`).
    fn seek(&mut self, pos: usize) {
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

/// What a group is: the whole pattern, a capturing `\(…\)`, or a non-capturing `\%(…\)`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Paren {
    Top,
    Capture,
    NonCapture,
}

/// The lowered pattern.
struct Lowered {
    rust: String,
    zs: Vec<usize>,
    ze: Vec<usize>,
    /// `Some(true)` for a `\c` in the pattern, `Some(false)` for `\C` (and no `\c`), else `None`.
    case: Option<bool>,
    /// An item that matches a line break was read (Vim's `RF_HASNL`).
    has_nl: bool,
}

/// Lower a Vim pattern to Rust-`regex` syntax under a starting magic level. `ci` is the final
/// case-insensitivity (it changes how a collection holding `[:upper:]`/`[:lower:]` is emitted).
fn lower(pattern: &str, magic0: Magic, ci: bool) -> Result<Lowered, RegexError> {
    let mut chars: Vec<char> = pattern.chars().collect();
    // `\%#=0` / `\%#=1` / `\%#=2` at the very start selects Vim's regexp engine. ruse has one engine
    // (design §7), so the selector is an accepted no-op; anything else after `\%#=` is Vim's E864.
    if chars.starts_with(&['\\', '%', '#', '=']) {
        if matches!(chars.get(4), Some('0'..='2')) {
            chars.drain(..5);
        } else {
            return Err(RegexError::Syntax(
                "E864: \\%#= can only be followed by 0, 1, or 2".into(),
            ));
        }
    }
    let mut p = Parser {
        lx: Lexer::new(&chars, magic0),
        groups: 0,
        zs: Vec::new(),
        ze: Vec::new(),
        captures: 0,
        saw_ic: false,
        saw_noic: false,
        ci,
        has_nl: false,
    };
    let rust = p.reg(Paren::Top)?;
    let case = if p.saw_ic {
        Some(true)
    } else if p.saw_noic {
        Some(false)
    } else {
        None
    };
    Ok(Lowered {
        rust,
        zs: p.zs,
        ze: p.ze,
        case,
        has_nl: p.has_nl,
    })
}

fn syntax(msg: &str) -> RegexError {
    RegexError::Syntax(msg.to_string())
}

/// The parser: Vim's `nfa_reg` / `nfa_regbranch` / `nfa_regconcat` / `nfa_regpiece` / `nfa_regatom`,
/// emitting Rust syntax instead of NFA postfix.
struct Parser<'a> {
    lx: Lexer<'a>,
    /// Rust capture groups opened so far (Vim's groups plus the `\zs`/`\ze` markers).
    groups: usize,
    /// Vim's own `\(` groups opened so far (at most 9, Vim's `NSUBEXP - 1`).
    captures: usize,
    zs: Vec<usize>,
    ze: Vec<usize>,
    saw_ic: bool,
    saw_noic: bool,
    ci: bool,
    /// An item that matches a line break was read (Vim's `RF_HASNL`).
    has_nl: bool,
}

/// One parsed atom and whether it is a `\zs`/`\ze` marker (which cannot take a multi).
struct Atom {
    rust: String,
    marker: bool,
}

impl Atom {
    fn of(rust: impl Into<String>) -> Atom {
        Atom {
            rust: rust.into(),
            marker: false,
        }
    }
}

impl Parser<'_> {
    /// `pattern ::= branch ( \| branch )*`, then the group terminator.
    fn reg(&mut self, paren: Paren) -> Result<String, RegexError> {
        let mut out = self.branch()?;
        while self.lx.peek() == Tok::Op('|') {
            self.lx.skip();
            out.push('|');
            out.push_str(&self.branch()?);
        }
        if paren == Paren::Top {
            match self.lx.peek() {
                Tok::End => {}
                Tok::Op(')') => return Err(syntax("E55: Unmatched \\)")),
                _ => return Err(syntax("E873: proper termination error")),
            }
        } else if self.lx.get() != Tok::Op(')') {
            return Err(syntax(if paren == Paren::Capture {
                "E54: Unmatched \\("
            } else {
                "E53: Unmatched \\%("
            }));
        }
        Ok(out)
    }

    /// `branch ::= concat ( \& concat )*` — `\&` needs lookahead, so it is rejected.
    fn branch(&mut self) -> Result<String, RegexError> {
        let out = self.concat()?;
        if self.lx.peek() == Tok::Op('&') {
            return Err(RegexError::Unsupported(
                "\\& concat (needs lookahead; owned engine is post-MVP)",
            ));
        }
        Ok(out)
    }

    /// `concat ::= piece*`, folding the case flags and magic switches in as they appear.
    fn concat(&mut self) -> Result<String, RegexError> {
        let mut out = String::new();
        loop {
            match self.lx.peek() {
                Tok::End | Tok::Op('|' | '&' | ')') => break,
                Tok::Op('c') => {
                    self.saw_ic = true;
                    self.lx.skip_keepstart();
                }
                Tok::Op('C') => {
                    self.saw_noic = true;
                    self.lx.skip_keepstart();
                }
                Tok::Op(m @ ('v' | 'm' | 'M' | 'V')) => {
                    self.lx.magic = match m {
                        'v' => Magic::Very,
                        'm' => Magic::Magic,
                        'M' => Magic::NoMagic,
                        _ => Magic::VeryNoMagic,
                    };
                    self.lx.skip_keepstart();
                }
                Tok::Op('Z') => {
                    return Err(RegexError::Unsupported(
                        "\\Z ignore-combining-characters (owned engine is post-MVP)",
                    ))
                }
                _ => out.push_str(&self.piece()?),
            }
        }
        Ok(out)
    }

    /// `piece ::= atom multi?`.
    fn piece(&mut self) -> Result<String, RegexError> {
        let atom = self.atom()?;
        let op = self.lx.peek();
        if !is_multi(op) {
            return Ok(atom.rust);
        }
        // Vim's `re_mult_next` refuses only the REPEATING multis (`*` `+` `\{`) on `\zs`/`\ze`; `\=`
        // `\?` (and `\@`) are accepted.
        if atom.marker && matches!(op, Tok::Op('*' | '+' | '{')) {
            return Err(syntax("E888: cannot repeat \\zs / \\ze"));
        }
        self.lx.skip();
        let quant = match op {
            Tok::Op('*') => "*".to_string(),
            Tok::Op('+') => "+".to_string(),
            Tok::Op('=' | '?') => "?".to_string(),
            Tok::Op('@') => return Err(self.lookaround()),
            _ => self.braces()?,
        };
        if is_multi(self.lx.peek()) {
            return Err(syntax("E871: Can't have a multi follow a multi"));
        }
        Ok(format!("(?:{}){quant}", atom.rust))
    }

    /// `\@=` `\@!` `\@<=` `\@<!` `\@>` (with an optional byte limit): all need lookaround.
    fn lookaround(&mut self) -> RegexError {
        let mut p = self.lx.pos;
        while self.lx.s.get(p).is_some_and(char::is_ascii_digit) {
            p += 1;
        }
        self.lx.seek(p);
        let kind = match self.lx.get().ch() {
            Some('=' | '!' | '>') => true,
            Some('<') => matches!(self.lx.get().ch(), Some('=' | '!')),
            _ => false,
        };
        if kind {
            RegexError::Unsupported(
                "lookaround \\@= \\@! \\@<= \\@<! \\@> (owned engine is post-MVP)",
            )
        } else {
            syntax("E869: Unknown operator after \\@")
        }
    }

    /// `\{n,m}` and friends (Vim's `read_limits`), already past the `{`. Returns the Rust quantifier.
    fn braces(&mut self) -> Result<String, RegexError> {
        const UNBOUNDED: u64 = u64::MAX;
        let s = self.lx.s;
        let mut p = self.lx.pos;
        let lazy = s.get(p) == Some(&'-');
        if lazy {
            p += 1;
        }
        // `getdigits`: leading blanks are skipped (strtoimax); no digits means the default.
        let digits = |p: &mut usize, default: u64| -> u64 {
            let mut q = *p;
            while s.get(q).is_some_and(|c| c.is_ascii_whitespace()) {
                q += 1;
            }
            let from = q;
            let mut n: u64 = 0;
            while let Some(d) = s.get(q).and_then(|c| c.to_digit(10)) {
                n = n.saturating_mul(10).saturating_add(u64::from(d));
                q += 1;
            }
            if q == from {
                default
            } else {
                *p = q;
                n
            }
        };
        let first = p;
        let mut min = digits(&mut p, 0);
        let mut max = if s.get(p) == Some(&',') {
            p += 1;
            if s.get(p).is_some_and(char::is_ascii_digit) {
                digits(&mut p, UNBOUNDED)
            } else {
                UNBOUNDED
            }
        } else if s.get(first).is_some_and(char::is_ascii_digit) {
            min
        } else {
            UNBOUNDED
        };
        if s.get(p) == Some(&'\\') {
            p += 1; // `\{…\}` and `\{…}` both close
        }
        if s.get(p) != Some(&'}') {
            return Err(syntax("E554: Syntax error in \\{...}"));
        }
        self.lx.seek(p + 1);
        self.lx.prev = Some(Tok::Lit('}'));
        if min > max {
            std::mem::swap(&mut min, &mut max);
        }
        let lazy = if lazy { "?" } else { "" };
        Ok(match (min, max) {
            (0, UNBOUNDED) => format!("*{lazy}"),
            (n, UNBOUNDED) => format!("{{{n},}}{lazy}"),
            (n, m) if n == m => format!("{{{n}}}"),
            (n, m) => format!("{{{n},{m}}}{lazy}"),
        })
    }

    /// Vim's `nfa_regatom`.
    fn atom(&mut self) -> Result<Atom, RegexError> {
        let t = self.lx.get();
        Ok(Atom::of(match t {
            Tok::End => return Err(syntax("E865: Regexp end encountered prematurely")),
            Tok::Op('^') => "(?m:^)".to_string(),
            Tok::Op('$') => "(?m:$)".to_string(),
            // Start/end of word — an APPROXIMATION, knowingly kept. Rust's Unicode word boundary
            // agrees with Vim between a word and a non-word character for ASCII and Latin letters.
            // It does NOT see Vim's other boundary: a change of `utf_class` between two word
            // characters (Latin to Hangul/CJK/kana/emoji), so `\<한` on "abc한글" is nvim (3,6) but
            // ruse no match. It also differs on Latin-1 × ÷ (keyword characters in Vim, not in
            // Unicode `\w`). Expressing the class change needs a look-behind at the previous
            // character, which the wrapped engine lacks; the owned engine is post-MVP.
            Tok::Op('<') => r"\b{start}".to_string(),
            Tok::Op('>') => r"\b{end}".to_string(),
            Tok::Op('_') => match self.lx.get().ch() {
                None => return Err(syntax("E865: Regexp end encountered prematurely")),
                Some('^') => "(?m:^)".to_string(),
                Some('$') => "(?m:$)".to_string(),
                Some('[') => match self.collection(true)? {
                    Some(c) => {
                        self.has_nl = true;
                        c
                    }
                    None => regex::escape("["), // no closing `]`: a literal `[`, as Vim reads it
                },
                Some(c) if CLASS_CHARS.contains(c) => {
                    self.has_nl = true;
                    class_atom(c, true, self.ci)
                }
                Some(c) => return Err(syntax(&format!("E877: Invalid character class: \\_{c}"))),
            },
            Tok::Op(c) if CLASS_CHARS.contains(c) => class_atom(c, false, self.ci),
            Tok::Op('n') => {
                self.has_nl = true;
                r"\n".to_string()
            }
            Tok::Op('(') => {
                self.captures += 1;
                if self.captures > 9 {
                    return Err(syntax("E51: Too many \\("));
                }
                self.groups += 1;
                format!("({})", self.reg(Paren::Capture)?)
            }
            Tok::Op(c @ ('|' | '&' | ')' | '=' | '?' | '+' | '@' | '*' | '{')) => {
                return Err(syntax(&format!("E866: Misplaced {c}")))
            }
            Tok::Op('~') => {
                return Err(RegexError::Unsupported(
                    "~ previous substitute string (not available to the matcher)",
                ))
            }
            Tok::Op('1'..='9') => {
                return Err(RegexError::Unsupported(
                    "backreference \\1-\\9 (owned engine is post-MVP)",
                ))
            }
            Tok::Op('z') => return self.z_atom(),
            Tok::Op('%') => self.percent()?,
            Tok::Op('[') => match self.collection(false)? {
                Some(c) => c,
                None => regex::escape("["),
            },
            // Every other character, magic or not, is itself (Vim's default case).
            Tok::Op(c) | Tok::Lit(c) => literal(c, self.ci),
        }))
    }

    /// `\zs` / `\ze`: an empty capture group marking the reported boundary.
    fn z_atom(&mut self) -> Result<Atom, RegexError> {
        let which = self.lx.get().ch();
        let list = match which {
            Some('s') => &mut self.zs,
            Some('e') => &mut self.ze,
            Some('1'..='9') => return Err(syntax("E67: \\z1 - \\z9 not allowed here")),
            Some('(') => return Err(syntax("E66: \\z( not allowed here")),
            _ => return Err(syntax("E867: Unknown operator after \\z")),
        };
        self.groups += 1;
        list.push(self.groups);
        Ok(Atom {
            rust: "()".to_string(),
            marker: true,
        })
    }

    /// The `\%` family.
    fn percent(&mut self) -> Result<String, RegexError> {
        let c = self.lx.get().ch();
        Ok(match c {
            Some('(') => format!("(?:{})", self.reg(Paren::NonCapture)?),
            Some(k @ ('d' | 'o' | 'x' | 'u' | 'U')) => {
                let (n, after) = char_code(self.lx.s, self.lx.pos, k)
                    .ok_or_else(|| syntax("E678: Invalid character after \\%[dxouU]"))?;
                self.lx.seek(after);
                let ch = char::from_u32(n).ok_or_else(|| {
                    syntax("E1541: Value too large, max Unicode codepoint is U+10FFFF")
                })?;
                literal(ch, self.ci)
            }
            Some('[') => self.optional_sequence()?,
            Some('#') => {
                let s = self.lx.s;
                let p = self.lx.pos;
                if s.get(p) == Some(&'=') && matches!(s.get(p + 1), Some('0'..='2')) {
                    return Err(syntax(
                        "E1281: Atom '\\%#=' must be at the start of the pattern",
                    ));
                }
                return Err(RegexError::Unsupported("\\%# cursor position atom"));
            }
            Some('^' | '$') => {
                return Err(RegexError::Unsupported(
                    "\\%^ / \\%$ start/end of file (needs buffer context)",
                ))
            }
            Some('V') => return Err(RegexError::Unsupported("\\%V visual-area atom")),
            Some('C') => return Err(RegexError::Unsupported("\\%C combining-character atom")),
            Some('<' | '>' | '.' | '\'' | '0'..='9') => {
                return Err(RegexError::Unsupported(
                    "\\%23l / \\%23c / \\%23v / \\%'m position atoms (need buffer context)",
                ))
            }
            _ => return Err(syntax("E867: Unknown operator after \\%")),
        })
    }

    /// `\%[abc]`: a sequence of optionally matched atoms, lowered to nested optionals
    /// `(?:a(?:b(?:c)?)?)?` (design §3).
    fn optional_sequence(&mut self) -> Result<String, RegexError> {
        let mut atoms = Vec::new();
        loop {
            match self.lx.peek() {
                Tok::Lit(']') => break,
                Tok::End => return Err(syntax("E69: Missing ] after \\%[")),
                _ => atoms.push(self.atom()?.rust),
            }
        }
        self.lx.skip();
        if atoms.is_empty() {
            return Err(syntax("E70: Empty \\%[]"));
        }
        Ok(atoms
            .iter()
            .rev()
            .fold(String::new(), |inner, a| format!("(?:{a}{inner})?")))
    }

    /// A collection `[...]`, already past the `[` (Vim's collection reader in `nfa_regatom`). `extra`
    /// is the `\_[` form, which also matches a line break. `Ok(None)`: no closing `]`, so the `[` is a
    /// literal.
    fn collection(&mut self, extra: bool) -> Result<Option<String>, RegexError> {
        let s = self.lx.s;
        let start = self.lx.pos;
        let endp = skip_anyof(s, start);
        if s.get(endp) != Some(&']') {
            return Ok(None);
        }
        // A `\n` item lets the collection match a line break, which makes the pattern multi-line.
        self.has_nl |= s[start..endp].windows(2).any(|w| w == ['\\', 'n']);
        if let Some(class) = recognized_class(s, start, endp, extra, self.ci) {
            self.lx.seek(endp + 1);
            return Ok(Some(class));
        }
        let mut set = CollSet::default();
        let mut i = start;
        let negated = s.get(i) == Some(&'^');
        if negated {
            i += 1;
        }
        // Vim tracks the previous item as an int: -1 = none, NEWL = a `\n` line-break item.
        const NONE: i64 = -1;
        const NEWL: i64 = -2;
        let mut startc: i64 = NONE;
        if s.get(i) == Some(&'-') {
            startc = '-' as i64;
            set.push_char('-');
            i += 1;
        }
        let mut emit_range = false;
        while i < endp {
            let oldstartc = startc;
            startc = NONE;
            if s[i] == '[' {
                if let Some((class, len)) = char_class_at(s, i) {
                    set.push_class(class);
                    i += len;
                    continue;
                }
                if equi_class_at(s, i).is_some() {
                    return Err(RegexError::Unsupported("equivalence class [=a=]"));
                }
                if let Some((c, len)) = coll_element_at(s, i) {
                    startc = c as i64;
                    i += len; // (Vim then also steps over the next char below — kept)
                }
            }
            // A range like `a-x`; a `-` with no start before it is a literal.
            if s.get(i) == Some(&'-') && oldstartc != NONE {
                emit_range = true;
                startc = oldstartc;
                i += 1;
                continue;
            }
            if s.get(i) == Some(&'\\')
                && i < endp
                && s.get(i + 1)
                    .is_some_and(|&n| IN_RANGE.contains(n) || ABBR.contains(n))
            {
                i += 1;
                match s[i] {
                    'n' => {
                        startc = if emit_range || s.get(i + 1) == Some(&'-') {
                            '\n' as i64
                        } else {
                            NEWL
                        };
                    }
                    k @ ('d' | 'o' | 'x' | 'u' | 'U') => match char_code(s, i + 1, k) {
                        Some((n, after)) => {
                            startc = i64::from(n);
                            i = after - 1;
                        }
                        // Not a number: the backslash is a literal and the letter is read next.
                        None => {
                            startc = '\\' as i64;
                            i -= 1;
                        }
                    },
                    c => startc = backslash_trans(c) as i64,
                }
            }
            if startc == NONE {
                startc = s.get(i).map_or(NONE, |&c| c as i64);
            }
            if emit_range {
                let (lo, hi) = (oldstartc, startc);
                if lo > hi {
                    return Err(syntax("E944: Reverse range in character class"));
                }
                set.push_range(to_char(lo)?, to_char(hi)?);
                emit_range = false;
                startc = NONE;
            } else if startc == NEWL {
                set.newline |= !negated; // a line break cannot be part of a negated collection
            } else if startc != NONE {
                set.push_char(to_char(startc)?);
            }
            i += 1;
        }
        // A `-` just before the `]` is a literal.
        if i > 0 && s.get(i - 1) == Some(&'-') {
            set.push_char('-');
        }
        self.lx.seek(endp + 1);
        set.emit(negated, extra, self.ci).map(Some)
    }
}

/// A literal character, under 'ignorecase' (`ci`) as the class of every character nvim's `utf_fold`
/// makes equal to it ([`crate::casemap::vim_fold_class`]).
fn literal(c: char, ci: bool) -> String {
    match crate::casemap::vim_fold_class(c) {
        Some(class) if ci => {
            let mut out = String::from("[");
            for &m in class {
                out.push_str(&format!(r"\x{{{m:X}}}"));
            }
            out.push(']');
            out
        }
        _ => regex::escape(&c.to_string()),
    }
}

fn to_char(n: i64) -> Result<char, RegexError> {
    u32::try_from(n)
        .ok()
        .and_then(char::from_u32)
        .ok_or_else(|| syntax("E1541: Value too large, max Unicode codepoint is U+10FFFF"))
}

/// Read the number of a `\%d123` / `\%o40` / `\%x2a` / `\%u20AC` / `\%U0001F600` item (or the `[\d…]`
/// form) starting at `p`. Returns the code and the index after the digits, `None` if there are none.
fn char_code(s: &[char], p: usize, kind: char) -> Option<(u32, usize)> {
    let (radix, max_len): (u32, usize) = match kind {
        'd' => (10, usize::MAX),
        'o' => (8, 3),
        'x' => (16, 2),
        'u' => (16, 4),
        _ => (16, 8),
    };
    let mut n: u64 = 0;
    let mut q = p;
    while q - p < max_len {
        // Octal stops once the value reaches 040 (so `\%o400` is `\%o40` + `0`), as in Vim.
        if kind == 'o' && n >= 0o40 {
            break;
        }
        let Some(d) = s.get(q).and_then(|c| c.to_digit(radix)) else {
            break;
        };
        n = n
            .saturating_mul(u64::from(radix))
            .saturating_add(u64::from(d));
        q += 1;
    }
    if q == p {
        return None;
    }
    Some((u32::try_from(n).unwrap_or(u32::MAX), q))
}

/// Where the pattern at the start of `s` ends: the byte offset of the `delim` that closes it, or
/// `s.len()` when none does (Vim's `skip_regexp`). A backslash escapes the next character, and a `[…]`
/// collection is skipped whole, so `[/]` in `:s/[/]/x/` or `/[/]` does not end the pattern. A bare `[`
/// opens a collection under `'magic'` (`magic`) or after `\v`; `\[` does under `'nomagic'` or after `\V`.
/// As in Vim, only the `\v` / `\V` switches are tracked here, not `\m` / `\M`. Ex commands and the
/// search command line use it to split a pattern from what follows it.
#[must_use]
pub fn skip_regexp(s: &str, delim: char, magic: bool) -> usize {
    // Vim's magic_T order: NONE (`\V`) < OFF < ON < ALL (`\v`).
    const NONE: u8 = 0;
    const OFF: u8 = 1;
    const ON: u8 = 2;
    const ALL: u8 = 3;
    let chars: Vec<char> = s.chars().collect();
    let mut level = if magic { ON } else { OFF };
    let mut p = 0;
    while p < chars.len() {
        if chars[p] == delim {
            break;
        }
        if (chars[p] == '[' && level >= ON)
            || (chars[p] == '\\' && chars.get(p + 1) == Some(&'[') && level <= OFF)
        {
            p = skip_anyof(&chars, p + 1);
            if p >= chars.len() {
                break;
            }
        } else if chars[p] == '\\' && p + 1 < chars.len() {
            p += 1;
            match chars[p] {
                'v' => level = ALL,
                'V' => level = NONE,
                _ => {}
            }
        }
        p += 1;
    }
    chars[..p.min(chars.len())]
        .iter()
        .map(|c| c.len_utf8())
        .sum()
}

/// Skip over a `[]` collection body starting just after the `[` (Vim's `skip_anyof`). Returns the index
/// of the closing `]`, or `s.len()` if there is none.
fn skip_anyof(s: &[char], mut p: usize) -> usize {
    if s.get(p) == Some(&'^') {
        p += 1;
    }
    if matches!(s.get(p), Some(']' | '-')) {
        p += 1;
    }
    while p < s.len() && s[p] != ']' {
        if s[p] == '-' {
            p += 1;
            if p < s.len() && s[p] != ']' {
                p += 1;
            }
        } else if s[p] == '\\'
            && s.get(p + 1)
                .is_some_and(|&n| IN_RANGE.contains(n) || ABBR.contains(n))
        {
            p += 2;
        } else if s[p] == '[' {
            if let Some((_, len)) = char_class_at(s, p) {
                p += len;
            } else if let Some(len) = equi_class_at(s, p) {
                p += len;
            } else if let Some((_, len)) = coll_element_at(s, p) {
                p += len;
            } else {
                p += 1;
            }
        } else {
            p += 1;
        }
    }
    p.min(s.len())
}

/// `[=a=]` at `p`: its length.
fn equi_class_at(s: &[char], p: usize) -> Option<usize> {
    (s.get(p + 1) == Some(&'=')
        && s.get(p + 2).is_some()
        && s.get(p + 3) == Some(&'=')
        && s.get(p + 4) == Some(&']'))
    .then_some(5)
}

/// `[.a.]` at `p`: the character and the length.
fn coll_element_at(s: &[char], p: usize) -> Option<(char, usize)> {
    (s.get(p + 1) == Some(&'.')
        && s.get(p + 2).is_some()
        && s.get(p + 3) == Some(&'.')
        && s.get(p + 4) == Some(&']'))
    .then(|| (s[p + 2], 5))
}

/// A `[:name:]` class: its Rust class body (valid inside `[...]`) and whether it must stay exact under
/// `'ignorecase'`. Vim tests a named class without case folding (`check_char_class`), so `[:upper:]`
/// does not match `a`, and no class that holds letters may pick up Rust's Unicode folds (KELVIN SIGN
/// U+212A for `k`, LONG S U+017F for `s`, ...). Letter-free classes fold to themselves either way.
#[derive(Clone, Copy)]
struct NamedClass {
    body: &'static str,
    case_sensitive: bool,
}

/// `[:name:]` at `p`: the class and the length of the item.
fn char_class_at(s: &[char], p: usize) -> Option<(NamedClass, usize)> {
    if s.get(p + 1) != Some(&':') {
        return None;
    }
    const TABLE: &[(&str, &str, bool)] = &[
        ("alnum", "0-9A-Za-z", true),
        ("alpha", "A-Za-z", true),
        ("backspace", r"\x{8}", false),
        ("blank", r"\x{20}\t", false),
        ("cntrl", r"\x{1}-\x{9}\x{B}-\x{1F}\x{7F}", false),
        ("digit", "0-9", false),
        ("escape", r"\x{1B}", false),
        ("fname", F_BODY, true),
        ("graph", r"\x{21}-\x{7E}", true),
        ("ident", I_BODY, true),
        ("keyword", K_BODY, true),
        ("lower", r"\p{Ll}", true),
        ("print", P_BODY, true),
        (
            "punct",
            r"\x{21}-\x{2F}\x{3A}-\x{40}\x{5B}-\x{60}\x{7B}-\x{7E}",
            false,
        ),
        ("return", r"\r", false),
        ("space", r"\t\x{B}-\x{D}\x{20}", false),
        ("tab", r"\t", false),
        ("upper", r"\p{Lu}", true),
        ("xdigit", "0-9A-Fa-f", true),
    ];
    let rest = &s[p + 2..];
    TABLE.iter().find_map(|&(name, body, case_sensitive)| {
        let n = name.chars().count();
        let matches = rest.len() >= n + 2
            && rest.iter().take(n).copied().eq(name.chars())
            && rest[n] == ':'
            && rest[n + 1] == ']';
        matches.then_some((
            NamedClass {
                body,
                case_sensitive,
            },
            n + 4,
        ))
    })
}

/// The items of one collection, as Rust class pieces.
#[derive(Default)]
struct CollSet {
    /// Pieces folded by `'ignorecase'` like any literal.
    items: String,
    /// `[:upper:]` / `[:lower:]`, which `'ignorecase'` does not fold.
    exact: String,
    /// A `\n` item: the collection also matches a line break.
    newline: bool,
    /// Some item covers U+000A, which a non-`\_` collection must not match (Vim never matches a line
    /// break that way).
    covers_lf: bool,
    /// The character and range items as code-point ranges, for closing the set under 'ignorecase'.
    ranges: Vec<(u32, u32)>,
}

impl CollSet {
    fn push_char(&mut self, c: char) {
        self.covers_lf |= c == '\n';
        self.items.push_str(&class_char(c));
        self.ranges.push((u32::from(c), u32::from(c)));
    }

    fn push_range(&mut self, lo: char, hi: char) {
        self.covers_lf |= lo <= '\n' && '\n' <= hi;
        self.items.push_str(&class_char(lo));
        self.items.push('-');
        self.items.push_str(&class_char(hi));
        self.ranges.push((u32::from(lo), u32::from(hi)));
    }

    /// Under 'ignorecase' nvim matches a collection item against any character with the same
    /// `utf_fold` (a range: any character folding like one inside it), so add every fold class that has
    /// a member among the items (#1112).
    fn fold_items(&mut self) {
        let inside = |x: u32| self.ranges.iter().any(|&(lo, hi)| lo <= x && x <= hi);
        let mut extra = String::new();
        for class in crate::casemap::vim_fold_classes() {
            if class.iter().any(|&m| inside(m)) {
                for &m in class.iter().filter(|&&m| !inside(m)) {
                    extra.push_str(&format!(r"\x{{{m:X}}}"));
                }
            }
        }
        self.items.push_str(&extra);
    }

    fn push_class(&mut self, class: NamedClass) {
        if class.case_sensitive {
            self.exact.push_str(class.body);
        } else {
            self.items.push_str(class.body);
        }
    }

    fn emit(mut self, negated: bool, extra: bool, ci: bool) -> Result<String, RegexError> {
        if ci {
            self.fold_items();
        }
        let split_exact = ci && !self.exact.is_empty();
        if negated {
            if split_exact {
                return Err(RegexError::Unsupported(
                    "negated collection holding a letter class ([:alpha:], [:upper:], ...) under 'ignorecase'",
                ));
            }
            let body = format!("{}{}", self.items, self.exact);
            return Ok(match (body.is_empty(), extra) {
                (true, true) => "(?s:.)".to_string(),
                (true, false) => r"[^\n]".to_string(),
                (false, true) => format!("[^{body}]"),
                (false, false) => format!(r"[^{body}\n]"),
            });
        }
        let body = if split_exact {
            self.items.clone()
        } else {
            format!("{}{}", self.items, self.exact)
        };
        let mut parts = Vec::new();
        if !body.is_empty() {
            parts.push(if self.covers_lf && !extra && !self.newline {
                format!(r"[{body}--\n]")
            } else {
                format!("[{body}]")
            });
        }
        if split_exact {
            parts.push(format!("(?-i:[{}])", self.exact));
        }
        if extra || self.newline {
            parts.push(r"\n".to_string());
        }
        Ok(match parts.len() {
            0 => r"[^\x{0}-\x{10FFFF}]".to_string(), // matches nothing
            1 => parts.pop().expect("one part"),
            _ => format!("(?:{})", parts.join("|")),
        })
    }
}

/// A character as a Rust class item, always in `\x{…}` form so no class metacharacter leaks.
fn class_char(c: char) -> String {
    format!(r"\x{{{:X}}}", u32::from(c))
}

// Vim's default option-driven classes (Unix defaults, as nvim v0.12.6 reports them), as Rust class
// bodies. `'isident'` / `'iskeyword'` = "@,48-57,_,192-255" (`@` adds µ U+00B5 in Latin-1); wide
// characters are keyword characters unless Vim's `utf_class` puts them in the blank/punctuation
// table, and emoji are keyword characters. `'isfname'` = "@,48-57,/,.,-,_,+,,,#,$,%,~,=" plus every
// non-ASCII character from U+00A0. `'isprint'` = "@,161-255" plus every wide character Vim prints.
/// Wide characters (U+0100 and up) that are keyword characters: all but Vim's blank/punctuation
/// `utf_class` table entries, with emoji (Extended_Pictographic, regional indicators) put back.
macro_rules! k_wide {
    () => {
        concat!(
    r"[\x{100}-\x{10FFFF}--[",
    r"\x{37E}\x{387}\x{55A}-\x{55F}\x{589}\x{5BE}\x{5C0}\x{5C3}\x{5F3}-\x{5F4}\x{60C}\x{61B}",
    r"\x{61F}\x{66A}-\x{66D}\x{6D4}\x{700}-\x{70D}\x{964}-\x{965}\x{970}\x{DF4}\x{E4F}",
    r"\x{E5A}-\x{E5B}\x{F04}-\x{F12}\x{F3A}-\x{F3D}\x{F85}\x{104A}-\x{104F}\x{10FB}",
    r"\x{1361}-\x{1368}\x{166D}-\x{166E}\x{1680}\x{169B}-\x{169C}\x{16EB}-\x{16ED}",
    r"\x{1735}-\x{1736}\x{17D4}-\x{17DC}\x{1800}-\x{180A}\x{2000}-\x{206F}\x{20A0}-\x{27FF}",
    r"\x{2900}-\x{2998}\x{29D8}-\x{29DB}\x{29FC}-\x{29FD}\x{2E00}-\x{2E7F}\x{3000}-\x{3020}",
    r"\x{3030}\x{303D}\x{FD3E}-\x{FD3F}\x{FE30}-\x{FE6B}\x{FF00}-\x{FF0F}\x{FF1A}-\x{FF20}",
    r"\x{FF3B}-\x{FF40}\x{FF5B}-\x{FF65}\x{1D000}-\x{1D24F}\x{1D400}-\x{1D7FF}",
    r"\x{1F000}-\x{1F2FF}\x{1F300}-\x{1F9FF}",
    r"]]",
    r"[\p{Extended_Pictographic}--\x{0}-\x{FF}]\x{1F1E6}-\x{1F1FF}",
        )
    };
}

/// Wide characters Vim prints (`utf_printable`): all but its non-printable table.
macro_rules! p_wide {
    () => {
        concat!(
            r"[\x{100}-\x{10FFFF}--[\x{70F}\x{180B}-\x{180E}\x{200B}-\x{200F}\x{202A}-\x{202E}",
            r"\x{2060}-\x{206F}\x{FEFF}\x{FFF9}-\x{FFFB}\x{FFFE}-\x{FFFF}]]",
        )
    };
}

const I_BODY: &str = r"0-9A-Za-z_\x{B5}\x{C0}-\x{FF}";
const I_NODIGIT: &str = r"A-Za-z_\x{B5}\x{C0}-\x{FF}";
const K_BODY: &str = concat!(r"0-9A-Za-z_\x{B5}\x{C0}-\x{FF}", k_wide!());
const K_NODIGIT: &str = concat!(r"A-Za-z_\x{B5}\x{C0}-\x{FF}", k_wide!());
const F_BODY: &str = r"\x{23}-\x{25}\x{2B}-\x{39}\x{3D}A-Z_a-z\x{7E}\x{A0}-\x{10FFFF}";
const F_NODIGIT: &str = r"\x{23}-\x{25}\x{2B}-\x{2F}\x{3D}A-Z_a-z\x{7E}\x{A0}-\x{10FFFF}";
const P_BODY: &str = concat!(r"\x{20}-\x{7E}\x{A0}-\x{FF}", p_wide!());
const P_NODIGIT: &str = concat!(r"\x{20}-\x{2F}\x{3A}-\x{7E}\x{A0}-\x{FF}", p_wide!());

/// A class atom (`.` or `\i \I … \u \U`), optionally with the `\_` newline form. Vim's classes are
/// ASCII (or Latin-1 for the option-driven ones); a negated class never matches a line break unless
/// it is the `\_` form. `\l \L \u \U` keep their case under `'ignorecase'`.
fn class_atom(c: char, newline: bool, ci: bool) -> String {
    if c == '.' {
        return if newline { "(?s:.)" } else { r"[^\n]" }.to_string();
    }
    let (body, negated): (String, bool) = match c {
        'i' => (I_BODY.into(), false),
        'I' => (I_NODIGIT.into(), false),
        'k' => (K_BODY.into(), false),
        'K' => (K_NODIGIT.into(), false),
        'f' => (F_BODY.into(), false),
        'F' => (F_NODIGIT.into(), false),
        'p' => (P_BODY.into(), false),
        'P' => (P_NODIGIT.into(), false),
        's' | 'S' => (r"\x{20}\t".into(), c == 'S'),
        'd' | 'D' => ("0-9".into(), c == 'D'),
        'x' | 'X' => ("0-9A-Fa-f".into(), c == 'X'),
        'o' | 'O' => ("0-7".into(), c == 'O'),
        'w' | 'W' => ("0-9A-Za-z_".into(), c == 'W'),
        'h' | 'H' => ("A-Za-z_".into(), c == 'H'),
        'a' | 'A' => ("A-Za-z".into(), c == 'A'),
        'l' | 'L' => ("a-z".into(), c == 'L'),
        _ => ("A-Z".into(), c == 'U'), // 'u' | 'U'
    };
    exact_class(&body, negated, newline, ci)
}

/// A class over `body` (negated or not, with or without the line break), kept EXACT under
/// `'ignorecase'`: Vim never case-folds a class atom (`\a`, `\w` do not match KELVIN SIGN U+212A or
/// LONG S U+017F; `\u` does not match `a`), so the class is wrapped in `(?-i:…)` when folding is on.
fn exact_class(body: &str, negated: bool, newline: bool, ci: bool) -> String {
    let class = match (negated, newline) {
        (false, false) => format!("[{body}]"),
        (false, true) => format!(r"[{body}\n]"),
        (true, false) => format!(r"[^{body}\n]"),
        (true, true) => format!("[^{body}]"),
    };
    if ci {
        format!("(?-i:{class})")
    } else {
        class
    }
}

/// Vim's `nfa_recognize_char_class`: a collection spelled exactly as a set of the ranges `0-9` `0-7`
/// `a-z` `A-Z` `a-f` `A-F`, `_` and `\n` (e.g. `[a-z]`, `[^0-9]`, `[A-Za-z_]`, `[0-9a-fA-F]`) is
/// compiled as the matching class atom, which is ASCII-only under `'ignorecase'` — `[a-z]` then
/// matches `a-zA-Z` but not KELVIN SIGN, while a collection that is not recognised (`[a-y]`,
/// `[a-z0-9]`) folds like a literal and does. `None`: not one of those spellings.
fn recognized_class(
    s: &[char],
    start: usize,
    endp: usize,
    extra: bool,
    ci: bool,
) -> Option<String> {
    const O9: u8 = 1;
    const O7: u8 = 2;
    const LZ: u8 = 4;
    const UZ: u8 = 8;
    const LF: u8 = 16;
    const UF: u8 = 32;
    const US: u8 = 64;
    let mut p = start;
    let negated = s.get(p) == Some(&'^');
    if negated {
        p += 1;
    }
    let mut newline = extra;
    let mut config = 0u8;
    while p < endp {
        if p + 2 < endp && s[p + 1] == '-' {
            config |= match (s[p], s[p + 2]) {
                ('0', '9') => O9,
                ('0', '7') => O7,
                ('a', 'z') => LZ,
                ('a', 'f') => LF,
                ('A', 'Z') => UZ,
                ('A', 'F') => UF,
                _ => return None,
            };
            p += 3;
        } else if p + 1 < endp && s[p] == '\\' && s[p + 1] == 'n' {
            newline = true;
            p += 2;
        } else if s[p] == '_' {
            config |= US;
            p += 1;
        } else if s[p] == '\n' {
            newline = true;
            p += 1;
        } else {
            return None;
        }
    }
    let body = match config {
        O9 => "0-9",
        x if x == LF | UF | O9 => "0-9A-Fa-f",
        O7 => "0-7",
        x if x == LZ | UZ | O9 | US => "0-9A-Za-z_",
        x if x == LZ | UZ | US => "A-Za-z_",
        x if x == LZ | UZ => "A-Za-z",
        // `\l` / `\u` with Vim's `_IC` twist: under 'ignorecase' they take both ASCII cases.
        LZ if ci => "A-Za-z",
        LZ => "a-z",
        UZ if ci => "A-Za-z",
        UZ => "A-Z",
        _ => return None,
    };
    Some(exact_class(body, negated, newline, ci))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pat: &str, hay: &str) -> Option<(usize, usize)> {
        let re = Regex::compile(pat, Options::default()).expect("compiles");
        re.find_at(hay, 0).map(|m| (m.start, m.end))
    }

    /// #803: the pattern ends at the delimiter Vim's `skip_regexp` finds — not one inside `[...]`, not an
    /// escaped one; a `[` with no `]` runs to the end; `\v` / `\V` decide whether `[` opens a collection.
    #[test]
    fn skip_regexp_steps_over_collections() {
        let cases: &[(&str, char, bool, usize)] = &[
            ("a/b", '/', true, 1),
            ("[/]/x/", '/', true, 3),
            (r"a\/b/c", '/', true, 4),
            ("[/x/", '/', true, 4),
            (r"\v[/]/x", '/', true, 5),
            (r"\V[/]/x", '/', true, 3),
            (r"\[/]/x", '/', false, 4),
            ("[]/]/x", '/', true, 4),
            ("[[:alpha:]/]/x", '/', true, 12),
            ("é/x", '/', true, "é".len()),
            ("abc", '/', true, 3),
        ];
        for &(s, delim, magic, want) in cases {
            assert_eq!(skip_regexp(s, delim, magic), want, "{s:?}");
        }
    }

    /// Only a pattern with a line-break item is multi-line (Vim's `RF_HASNL`).
    #[test]
    fn multiline_marks_line_break_items() {
        let ml = |p: &str| Regex::compile(p, Options::default()).unwrap().multiline();
        assert!(ml(r"a\nb") && ml(r"\_s") && ml(r"\_[ab]") && ml(r"[a\n]"));
        assert!(!ml("ab") && !ml(r"a\s") && !ml("[ab]") && !ml(r"\_^a"));
    }

    #[test]
    fn literal_and_dot_star() {
        assert_eq!(find("foo", "a foo b"), Some((2, 5)));
        assert_eq!(find("f.o", "a fxo b"), Some((2, 5)));
        assert_eq!(find("a.*b", "xaZZbx"), Some((1, 5)));
    }

    /// Default magic (`\m`): `(` `+` are LITERAL; `\(` `\+` are the operators — the inversion the
    /// front-end owns.
    #[test]
    fn default_magic_literalness() {
        // bare `(` is a literal paren
        assert_eq!(find("f(x", "a f(x b"), Some((2, 5)));
        // bare `+` is a literal plus
        assert_eq!(find("a+", "xa+y"), Some((1, 3)));
        // `\+` is one-or-more
        assert_eq!(find("a\\+", "xaaay"), Some((1, 4)));
        // `\(ab\)\+` groups and repeats
        assert_eq!(find("\\(ab\\)\\+", "zababz"), Some((1, 5)));
    }

    /// Very-magic (`\v`): `(` `+` are operators bare, like Rust/PCRE.
    #[test]
    fn very_magic() {
        assert_eq!(find("\\v(ab)+", "zababz"), Some((1, 5)));
        assert_eq!(find("\\va+", "xaaay"), Some((1, 4)));
    }

    /// Very-nomagic (`\V`): everything literal — `.` matches a real dot only.
    #[test]
    fn very_nomagic_all_literal() {
        assert_eq!(find("\\Va.b", "xaxbx"), None); // `.` is literal here
        assert_eq!(find("\\Va.b", "xa.bx"), Some((1, 4)));
    }

    /// `\zs` resets the reported match START; `\ze` the END. The context still has to match, but the
    /// reported span is only the middle — the atom with no PCRE equivalent (design §4).
    #[test]
    fn no_match_in_the_slot_after_the_final_newline() {
        let re = Regex::compile("^", Options::default()).unwrap();
        let hay = "abc\ndef\n";
        assert_eq!(re.find_at(hay, 1).map(|m| m.start), Some(4));
        assert_eq!(re.find_at(hay, 5), None);
        let starts: Vec<usize> = re.find_all(hay).iter().map(|m| m.start).collect();
        assert_eq!(starts, vec![0, 4]);
        let re = Regex::compile("^$", Options::default()).unwrap();
        assert_eq!(re.find_at(hay, 0), None);
        assert_eq!(re.find_at("abc\n\ndef\n", 5), None);
        // A haystack without a final newline (one line, an empty buffer) keeps its end match.
        assert_eq!(re.find_at("", 0), Some(Match { start: 0, end: 0 }));
        assert_eq!(find("$", "abc"), Some((3, 3)));
    }

    #[test]
    fn zs_ze_boundary_override() {
        // `foo\zsbar` reports only `bar`, but only when preceded by `foo`.
        assert_eq!(find("foo\\zsbar", "a foobar b"), Some((5, 8)));
        assert_eq!(find("foo\\zsbar", "a xxxbar b"), None);
        // `\zsbar\ze` — both boundaries.
        assert_eq!(find("o\\zsba\\zer", "a foobar b"), Some((5, 7)));
        // `\ze` only: `foo\ze` reports the empty span before... reports up to \ze = the `foo` start..\ze.
        assert_eq!(find("foo\\zebar", "a foobar b"), Some((2, 5)));
    }

    #[test]
    fn smartcase_upper_forces_sensitive() {
        let ci = Options {
            ignore_case: true,
            smart_case: true,
            ..Options::default()
        };
        // all-lowercase pattern → case-insensitive
        assert!(Regex::compile("foo", ci)
            .unwrap()
            .find_at("a FOO b", 0)
            .is_some());
        // an uppercase in the pattern → case-sensitive, so `FOO` won't match `foo`
        assert!(Regex::compile("Foo", ci)
            .unwrap()
            .find_at("a foo b", 0)
            .is_none());
    }

    #[test]
    fn find_all_non_overlapping() {
        let re = Regex::compile("a\\+", Options::default()).unwrap();
        let ms: Vec<_> = re
            .find_all("aa b aaa")
            .iter()
            .map(|m| (m.start, m.end))
            .collect();
        assert_eq!(ms, vec![(0, 2), (5, 8)]);
    }

    fn all(pat: &str, hay: &str) -> Vec<(usize, usize)> {
        Regex::compile(pat, Options::default())
            .expect("compiles")
            .find_all(hay)
            .iter()
            .map(|m| (m.start, m.end))
            .collect()
    }

    /// Regression for the `find_all` infinite loop on a zero-width match at end-of-string (a search/
    /// `hlsearch`/`:s///g` DoS reachable from `/a*`, `/$`, `/.*`, or empty `/`). Every one of these
    /// patterns matches empty at `hay.len()`; the old hand-rolled scan could not advance past it and
    /// spun forever. Each `all(...)` call below would hang, not fail, if the bug returned — so this test
    /// IS the termination gate. The expected spans are Vim-faithful, captured from nvim v0.12.4 via
    /// `:s/<pat>/-/g` (a dash marks each match; a trailing dash means a match at end-of-line):
    ///   "abc" + `x*`  -> "-a-b-c"      (empty before each char, NONE at EOL)
    ///   "aaa" + `a*`  -> "-"            (one whole-line match, no trailing empty)
    ///   "aax" + `a*`  -> "-x"           (only "aa"; no empty adjacent to it, none at EOL)
    ///   "abc" + `$`   -> "abc-"         (a lone anchored empty match AT EOL is kept)
    ///   "abc" + `.*`  -> "-"            (greedy whole-line match, no trailing empty)
    ///   ""    + `x*`  -> "-"            (the SOLE empty match on an empty line is kept)
    ///   "foo bar" + `o*` -> "-f- -b-a-r"
    #[test]
    fn find_all_zero_width_at_eol_terminates_and_matches_nvim() {
        assert_eq!(all("x*", "abc"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(all("a*", "aaa"), vec![(0, 3)]);
        assert_eq!(all("a*", "aax"), vec![(0, 2)]);
        assert_eq!(all("$", "abc"), vec![(3, 3)]);
        assert_eq!(all(".*", "abc"), vec![(0, 3)]);
        assert_eq!(all("x*", ""), vec![(0, 0)]);
        assert_eq!(
            all("o*", "foo bar"),
            vec![(0, 0), (1, 3), (4, 4), (5, 5), (6, 6)]
        );
    }

    /// `:s///g` resumes at the reported (`\ze`) end, and keeps an empty match at the end of a line that a
    /// match (not a one-character step) reached (#1047, #1048; nvim v0.12.6: `:s/\v(ab)\zeab/-/g` on
    /// "ababab" is "--ab", `:s/\>/-/g` on "a b" is "a- b-", `:s/a\|$/-/g` on "ab" is "-b-"). Each line of a
    /// multi-line haystack is scanned on its own.
    #[test]
    fn find_all_follows_do_sub() {
        assert_eq!(all(r"\v(ab)\zeab", "ababab"), vec![(0, 2), (2, 4)]);
        assert_eq!(all(r"\>", "a b"), vec![(1, 1), (3, 3)]);
        assert_eq!(all(r"\>", "foo bar"), vec![(3, 3), (7, 7)]);
        assert_eq!(all(r"a\|$", "ab"), vec![(0, 1), (2, 2)]);
        assert_eq!(all("x*", "ab\ncd"), vec![(0, 0), (1, 1), (3, 3), (4, 4)]);
        assert_eq!(all("^", "ab\ncd\n"), vec![(0, 0), (3, 3)]);
    }

    /// A `/` search sees every empty match (`\<` at each word start, `$` at each line end) and resumes at
    /// the reported end (#705).
    #[test]
    fn search_all_keeps_empty_matches() {
        let spans = |pat: &str, hay: &str| -> Vec<(usize, usize)> {
            Regex::compile(pat, Options::default())
                .expect("compiles")
                .search_all(hay)
                .iter()
                .map(|m| (m.start, m.end))
                .collect()
        };
        assert_eq!(spans(r"\<", "xx foo bar"), vec![(0, 0), (3, 3), (7, 7)]);
        assert_eq!(spans("$", "ab\ncd\n"), vec![(2, 2), (5, 5)]);
        assert_eq!(spans("^$", "a\n\nb"), vec![(2, 2)]);
        assert_eq!(spans("x*", "ab"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(spans(r"foo\zsbar", "foobar foobar"), vec![(3, 6), (10, 13)]);
        assert_eq!(spans("aa", "aaa"), vec![(0, 2)]);
        assert_eq!(spans(r"a\nb", "a\nb\na\nb"), vec![(0, 3), (4, 7)]);
    }

    /// Vim allows nine `\(` groups (E51 beyond); `\%(` and the `\zs` markers do not count.
    #[test]
    fn at_most_nine_capture_groups() {
        let nine = r"\(a\)".repeat(9);
        assert!(Regex::compile(&nine, Options::default()).is_ok());
        assert!(Regex::compile(&format!(r"{nine}\%(b\)\zs"), Options::default()).is_ok());
        assert!(matches!(
            Regex::compile(&format!(r"{nine}\(b\)"), Options::default()),
            Err(RegexError::Syntax(_))
        ));
    }

    /// A belt-and-suspenders wall-clock guard: even if the expectations above were ever loosened, a
    /// regressed `find_all` must never hang. Run the worst zero-width-at-EOL case in a worker thread and
    /// require it to finish well under a generous budget.
    #[test]
    fn find_all_zero_width_never_hangs() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let n = Regex::compile("a*", Options::default())
                .unwrap()
                .find_all("aaaa")
                .len();
            let _ = tx.send(n);
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "find_all did not terminate (zero-width-at-EOL infinite loop regressed)"
        );
    }

    #[test]
    fn non_greedy_star() {
        // `\{-}` is Vim's non-greedy `*` — `a.\{-}b` matches the SHORTEST `a…b`.
        let re = Regex::compile("a.\\{-}b", Options::default()).unwrap();
        assert_eq!(re.find_at("axbxb", 0), Some(Match { start: 0, end: 3 }));
    }

    /// Rejects, never fakes: unrepresentable atoms return a typed error (design §2).
    #[test]
    fn unsupported_atoms_are_rejected() {
        assert!(matches!(
            Regex::compile("\\(a\\)\\1", Options::default()),
            Err(RegexError::Unsupported(_))
        ));
        assert!(matches!(
            Regex::compile("foo\\@=", Options::default()),
            Err(RegexError::Unsupported(_))
        ));
        // A position atom needs buffer context the matcher does not have. (`\%(ab\)` used to be pinned
        // here as rejected; it is a plain non-capturing group, which nvim matches and ruse now lowers.)
        assert!(matches!(
            Regex::compile("\\%Vab", Options::default()),
            Err(RegexError::Unsupported(_))
        ));
        assert_eq!(find("\\%(ab\\)\\+", "xabab"), Some((1, 5)));
    }
}
