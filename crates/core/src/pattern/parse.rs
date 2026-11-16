//! The recursive-descent parser that lowers a Vim pattern to Rust-`regex` syntax (Neovim's `nfa_reg`
//! family; see the `pattern` module docs).

use super::classes::{class_atom, recognized_class};
use super::collection::{char_class_at, coll_element_at, equi_class_at, skip_anyof, CollSet};
use super::lex::{backslash_trans, is_multi, Lexer, Tok, ABBR, CLASS_CHARS, IN_RANGE};
use super::{Magic, RegexError};

/// What a group is: the whole pattern, a capturing `\(…\)`, or a non-capturing `\%(…\)`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Paren {
    Top,
    Capture,
    NonCapture,
}

/// The lowered pattern.
pub(super) struct Lowered {
    pub(super) rust: String,
    pub(super) zs: Vec<usize>,
    pub(super) ze: Vec<usize>,
    /// `Some(true)` for a `\c` in the pattern, `Some(false)` for `\C` (and no `\c`), else `None`.
    pub(super) case: Option<bool>,
    /// An item that matches a line break was read (Vim's `RF_HASNL`).
    pub(super) has_nl: bool,
    /// A `\<` opening the pattern was dropped from `rust` (checked after the match instead).
    pub(super) bow: bool,
    /// A `\>` closing the pattern was dropped from `rust`.
    pub(super) eow: bool,
    /// Zero-width assertions left in `rust` (`^`, `$`, a mid-pattern `\<` / `\>`).
    pub(super) asserts: usize,
    /// The pattern has a multi, an alternation or a `\%[`: a match at one start can end in more than
    /// one place.
    pub(super) variable: bool,
}

/// Lower a Vim pattern to Rust-`regex` syntax under a starting magic level. `ci` is the final
/// case-insensitivity (it changes how a collection holding `[:upper:]`/`[:lower:]` is emitted).
pub(super) fn lower(pattern: &str, magic0: Magic, ci: bool) -> Result<Lowered, RegexError> {
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
        edges: (false, false),
        asserts: 0,
        variable: false,
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
        bow: p.edges.0,
        eow: p.edges.1,
        asserts: p.asserts,
        variable: p.variable,
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
    /// The top-level `\<` / `\>` at the pattern's edges that were dropped (see [`Lowered::bow`]).
    edges: (bool, bool),
    /// See [`Lowered::asserts`].
    asserts: usize,
    /// See [`Lowered::variable`].
    variable: bool,
}

/// One parsed atom, whether it is a `\zs`/`\ze` marker (which cannot take a multi), and the `<` / `>`
/// of a word-edge atom.
struct Atom {
    rust: String,
    marker: bool,
    edge: Option<char>,
}

impl Atom {
    fn of(rust: impl Into<String>) -> Atom {
        Atom {
            rust: rust.into(),
            marker: false,
            edge: None,
        }
    }
}

/// One parsed piece: its Rust syntax and, for a bare `\<` / `\>` (no multi), which one.
struct Piece {
    rust: String,
    edge: Option<char>,
}

fn join(pieces: &[Piece]) -> String {
    pieces.iter().map(|p| p.rust.as_str()).collect()
}

impl Parser<'_> {
    /// `pattern ::= branch ( \| branch )*`, then the group terminator.
    ///
    /// At the top, a pattern of one branch that opens with `\<` or closes with `\>` drops that atom:
    /// Vim's word edge is a change of character class (`utf_class`), which the wrapped engine cannot
    /// see, so [`Regex`] checks it on each candidate instead (#745). Anywhere else the atom stays
    /// approximated by Rust's Unicode `\b{start}` / `\b{end}`.
    fn reg(&mut self, paren: Paren) -> Result<String, RegexError> {
        let mut pieces = self.branch()?;
        let mut alternatives = String::new();
        while self.lx.peek() == Tok::Op('|') {
            self.lx.skip();
            self.variable = true;
            alternatives.push('|');
            alternatives.push_str(&join(&self.branch()?));
        }
        if paren == Paren::Top && alternatives.is_empty() {
            if pieces.first().is_some_and(|p| p.edge == Some('<')) {
                pieces.remove(0);
                self.edges.0 = true;
                self.asserts -= 1;
            }
            if pieces.last().is_some_and(|p| p.edge == Some('>')) {
                pieces.pop();
                self.edges.1 = true;
                self.asserts -= 1;
            }
        }
        let out = join(&pieces) + &alternatives;
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
    fn branch(&mut self) -> Result<Vec<Piece>, RegexError> {
        let out = self.concat()?;
        if self.lx.peek() == Tok::Op('&') {
            return Err(RegexError::Unsupported(
                "\\& concat (needs lookahead; owned engine is post-MVP)",
            ));
        }
        Ok(out)
    }

    /// `concat ::= piece*`, folding the case flags and magic switches in as they appear.
    fn concat(&mut self) -> Result<Vec<Piece>, RegexError> {
        let mut out = Vec::new();
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
                _ => out.push(self.piece()?),
            }
        }
        Ok(out)
    }

    /// `piece ::= atom multi?`.
    fn piece(&mut self) -> Result<Piece, RegexError> {
        let atom = self.atom()?;
        let op = self.lx.peek();
        if !is_multi(op) {
            return Ok(Piece {
                rust: atom.rust,
                edge: atom.edge,
            });
        }
        self.variable = true;
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
        Ok(Piece {
            rust: format!("(?:{}){quant}", atom.rust),
            edge: None,
        })
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
        if matches!(t, Tok::Op('^' | '$' | '<' | '>'))
            || (t == Tok::Op('_') && matches!(self.lx.peek().ch(), Some('^' | '$')))
        {
            self.asserts += 1;
        }
        Ok(Atom::of(match t {
            Tok::End => return Err(syntax("E865: Regexp end encountered prematurely")),
            Tok::Op('^') => "(?m:^)".to_string(),
            Tok::Op('$') => "(?m:$)".to_string(),
            // Start/end of word. At the pattern's edges `reg` drops the atom and `Regex` checks Vim's
            // class rule on each candidate (#745). Anywhere else it is an APPROXIMATION, knowingly
            // kept: Rust's Unicode word boundary agrees with Vim between a word and a non-word
            // character for ASCII and Latin letters, but does NOT see Vim's other boundary, a change
            // of `utf_class` between two word characters (Latin to Hangul/CJK/kana/emoji: `x\<한`
            // on "x한" is nvim (0,4), ruse no match), and differs on Latin-1 × ÷ (keyword characters
            // in Vim, not in Unicode `\w`). Expressing it needs a look-behind at the previous
            // character, which the wrapped engine lacks; the owned engine (#1128) will.
            Tok::Op(c @ ('<' | '>')) => {
                return Ok(Atom {
                    rust: if c == '<' { r"\b{start}" } else { r"\b{end}" }.to_string(),
                    marker: false,
                    edge: Some(c),
                })
            }
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
            edge: None,
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
        self.variable = true;
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
