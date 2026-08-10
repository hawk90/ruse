//! C-REGEX: the Vim-dialect regex abstraction (D-028, `docs/design/vim-regex.md`).
//!
//! A magic-aware FRONT-END parses a Vim search pattern into Rust-`regex` syntax, resolving the magic
//! levels (`\v` very-magic / `\m` magic / `\M` nomagic / `\V` very-nomagic) exactly once and lowering
//! `\zs`/`\ze` match-boundary overrides onto a reserved capture group (design §4). The wrapped Rust
//! `regex` crate is the SOLE engine for MVP (DEP-REGEX: linear-time, ReDoS-immune); its types never
//! reach this module's public API (INV-CONTRACT-FIRST). Atoms the crate cannot represent — lookaround
//! (`\@=` `\@<=` …), backrefs (`\1`), `\%(`-family position atoms — are **rejected with a typed error,
//! never faked** (design §2). The owned NFA engine that would support them is post-MVP (scope decided
//! 2026-08-11: MVP ships the wrapped engine alone).
//!
//! What "magic" means, and why it is not a vocabulary difference: in the DEFAULT level `\m`, the bytes
//! `+ ? { } ( ) | < >` are LITERAL and must be backslash-escaped to become operators — the opposite of
//! Rust/PCRE, where they are operators bare. The front-end owns that inversion so the two never smear.

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
    /// An atom the MVP (wrapped-`regex`) engine cannot represent — lookaround, backrefs, `\%(`-family.
    /// Rejected, never approximated, so a real Vim pattern never silently matches the wrong span.
    Unsupported(&'static str),
    /// The lowered pattern was itself malformed (unbalanced group, bad `\{…}`, or the Rust engine
    /// rejected the compiled form). Carries the engine's message for the status line.
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

/// A compiled Vim-dialect pattern (C-REGEX). Holds the wrapped engine and the capture group that
/// carries the `\zs`/`\ze` reported span (if the pattern used either).
#[derive(Clone, Debug)]
pub struct Regex {
    inner: RawRegex,
    /// The capture group whose span is the REPORTED match, when `\zs`/`\ze` was used; `None` = the
    /// whole match is reported. Modeled as a reserved capture slot (design §4).
    boundary_group: Option<usize>,
}

impl Regex {
    /// Compile a Vim `pattern` under `opts`. Lowers the magic level + `\zs`/`\ze` to Rust syntax, then
    /// builds the wrapped engine.
    ///
    /// # Errors
    /// [`RegexError::Unsupported`] for a lookaround/backref/`\%`-atom; [`RegexError::Syntax`] for a
    /// malformed pattern or one the engine rejects.
    pub fn compile(pattern: &str, opts: Options) -> Result<Regex, RegexError> {
        let (lowered, boundary_group) = lower(pattern, opts.magic)?;
        // 'smartcase': an uppercase in the ORIGINAL pattern forces case-sensitivity (Vim looks at the
        // typed pattern, so `\zs`/escapes count as their letters — a plain has_uppercase is the rule).
        let case_insensitive =
            opts.ignore_case && !(opts.smart_case && pattern.chars().any(|c| c.is_uppercase()));
        let inner = regex::RegexBuilder::new(&lowered)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| RegexError::Syntax(e.to_string()))?;
        Ok(Regex {
            inner,
            boundary_group,
        })
    }

    /// The first match at a byte offset `>= from`, honoring the `\zs`/`\ze` reported span. `None` if
    /// there is no match from `from` to the end (the caller wraps for `/`-search).
    #[must_use]
    pub fn find_at(&self, hay: &str, from: usize) -> Option<Match> {
        let from = from.min(hay.len());
        // A capture-based match is needed only when a boundary group is in play.
        let caps = self.inner.captures_at(hay, from)?;
        Some(self.report(&caps))
    }

    /// Every non-overlapping match in `hay`, left to right (for `hlsearch` highlight and `:s///g`).
    #[must_use]
    pub fn find_all(&self, hay: &str) -> Vec<Match> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= hay.len() {
            let Some(caps) = self.inner.captures_at(hay, at) else {
                break;
            };
            let m = self.report(&caps);
            // Advance past this match; guard against a zero-width match stalling the scan.
            let whole = caps.get(0).expect("group 0 always present");
            at = if whole.end() > at {
                whole.end()
            } else {
                // Zero-width (or boundary-only) match: step one char to make progress.
                next_char_boundary(hay, whole.end())
            };
            out.push(m);
        }
        out
    }

    /// Resolve the reported span of a capture set: the `\zs`/`\ze` boundary group if present and it
    /// participated, else the whole match.
    fn report(&self, caps: &regex::Captures) -> Match {
        let span = self
            .boundary_group
            .and_then(|g| caps.get(g))
            .or_else(|| caps.get(0))
            .expect("group 0 always present");
        Match {
            start: span.start(),
            end: span.end(),
        }
    }
}

/// The next char boundary strictly after `i` (or `hay.len()`), so a zero-width match advances by one
/// user-visible character rather than one byte (never splitting a UTF-8 sequence).
fn next_char_boundary(hay: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < hay.len() && !hay.is_char_boundary(j) {
        j += 1;
    }
    j.min(hay.len())
}

/// Lower a Vim pattern to Rust-`regex` syntax under a starting magic level, returning the lowered
/// string and the capture group carrying the `\zs`/`\ze` reported span (if any).
fn lower(pattern: &str, magic0: Magic) -> Result<(String, Option<usize>), RegexError> {
    let chars: Vec<char> = pattern.chars().collect();
    // Locate the FIRST \zs / \ze (Vim allows repeats; MVP honors the first of each). Their char
    // positions decide where the reported-span capture group opens and closes.
    let zs = marker_pos(&chars, 's');
    let ze = marker_pos(&chars, 'e');
    let (open_at, close_at) = match (zs, ze) {
        (Some(s), Some(e)) => (Some(s), Some(e)),
        (Some(s), None) => (Some(s), Some(chars.len())), // \zs only: capture runs to the end
        (None, Some(e)) => (Some(0), Some(e)),           // \ze only: capture runs from the start
        (None, None) => (None, None),
    };

    let mut out = String::new();
    let mut magic = magic0;
    let mut groups = 0usize; // capture groups emitted so far (for the boundary-group index)
    let mut boundary_group = None;
    let mut i = 0usize;
    while i <= chars.len() {
        if Some(i) == open_at {
            out.push('(');
            groups += 1;
            boundary_group = Some(groups);
        }
        if Some(i) == close_at {
            out.push(')');
        }
        if i == chars.len() {
            break;
        }
        let c = chars[i];
        if c == '\\' {
            let next = chars.get(i + 1).copied();
            match next {
                // \zs / \ze: already handled as the boundary group; consume the marker.
                Some('z') if matches!(chars.get(i + 2), Some('s') | Some('e')) => {
                    i += 3;
                    continue;
                }
                // Magic-level switches consume with no output.
                Some('v') => {
                    magic = Magic::Very;
                    i += 2;
                    continue;
                }
                Some('V') => {
                    magic = Magic::VeryNoMagic;
                    i += 2;
                    continue;
                }
                Some('m') => {
                    magic = Magic::Magic;
                    i += 2;
                    continue;
                }
                Some('M') => {
                    magic = Magic::NoMagic;
                    i += 2;
                    continue;
                }
                Some(n) => {
                    i += 2 + lower_escape(n, &chars[i + 2..], &mut out, &mut groups)?;
                    continue;
                }
                None => {
                    out.push_str("\\\\"); // a trailing backslash is a literal backslash
                    i += 1;
                    continue;
                }
            }
        }
        lower_bare(c, magic, &mut out, &mut groups);
        i += 1;
    }

    if out.is_empty() {
        return Err(RegexError::Syntax("empty pattern".into()));
    }
    Ok((out, boundary_group))
}

/// Position (char index) of the FIRST `\zs` (kind `'s'`) or `\ze` (kind `'e'`) in `chars`.
fn marker_pos(chars: &[char], kind: char) -> Option<usize> {
    chars
        .windows(3)
        .position(|w| w[0] == '\\' && w[1] == 'z' && w[2] == kind)
}

/// Lower a backslash escape `\<n>` (with `rest` = the chars after it, for multi-char forms like `\{-}`).
/// Returns how many EXTRA chars past the `\<n>` pair were consumed. Appends to `out`. In `\m`/`\M` these
/// backslash forms are the OPERATORS; the front-end has already decided that by reaching here.
fn lower_escape(
    n: char,
    rest: &[char],
    out: &mut String,
    groups: &mut usize,
) -> Result<usize, RegexError> {
    match n {
        // Grouping / alternation / quantifiers (operators in magic; the backslash makes them so).
        '(' => {
            out.push('(');
            *groups += 1;
            Ok(0)
        }
        ')' => {
            out.push(')');
            Ok(0)
        }
        '|' => {
            out.push('|');
            Ok(0)
        }
        '+' => {
            out.push('+');
            Ok(0)
        }
        '?' | '=' => {
            out.push('?');
            Ok(0)
        }
        '}' => {
            out.push('}');
            Ok(0)
        }
        // `\{` opens a Vim count; `\{-}` / `\{-n,m}` is the NON-GREEDY form. Translate the `-` to a Rust
        // lazy quantifier suffix. The closing `}` may be `\}` or a bare `}`.
        '{' => {
            if rest.first() == Some(&'-') {
                // \{-...} → {...}?  (non-greedy). \{-} alone → *?
                let mut body = String::new();
                let mut k = 1; // past the '-'
                while k < rest.len() && rest[k] != '}' {
                    if rest[k] == '\\' {
                        k += 1;
                        continue;
                    }
                    body.push(rest[k]);
                    k += 1;
                }
                let consumed = if k < rest.len() { k + 1 } else { k }; // include closing '}'
                if body.is_empty() {
                    out.push_str("*?");
                } else {
                    out.push('{');
                    out.push_str(&body);
                    out.push('}');
                    out.push('?');
                }
                Ok(consumed)
            } else {
                out.push('{');
                Ok(0)
            }
        }
        // Word boundaries — Rust `regex` has no start/end-of-word anchor, so both lower to `\b`
        // (approximate; documented). `\<`/`\>` are not an F-009 acceptance atom.
        '<' | '>' => {
            out.push_str("\\b");
            Ok(0)
        }
        // Character classes (map the common Vim ones onto Rust equivalents).
        'w' | 'W' | 's' | 'S' | 'd' | 'D' => {
            out.push('\\');
            out.push(n);
            Ok(0)
        }
        'a' => {
            out.push_str("[A-Za-z]");
            Ok(0)
        }
        'l' => {
            out.push_str("[a-z]");
            Ok(0)
        }
        'u' => {
            out.push_str("[A-Z]");
            Ok(0)
        }
        'x' => {
            out.push_str("[0-9A-Fa-f]");
            Ok(0)
        }
        'o' => {
            out.push_str("[0-7]");
            Ok(0)
        }
        'h' => {
            out.push_str("[A-Za-z_]");
            Ok(0)
        }
        'n' => {
            out.push_str("\\n");
            Ok(0)
        }
        't' => {
            out.push_str("\\t");
            Ok(0)
        }
        '\\' => {
            out.push_str("\\\\");
            Ok(0)
        }
        // Backrefs and lookaround / position-atom families: unrepresentable in the wrapped engine.
        // Reject, never fake (design §2) — the owned NFA engine is post-MVP.
        '1'..='9' => Err(RegexError::Unsupported(
            "backreference \\1-\\9 (owned engine is post-MVP)",
        )),
        '@' => Err(RegexError::Unsupported(
            "lookaround \\@= \\@! \\@<= \\@<! (owned engine is post-MVP)",
        )),
        '%' => Err(RegexError::Unsupported(
            "\\%(…\\) / \\%[…] / position atoms (owned engine is post-MVP)",
        )),
        // Anything else backslash-escaped is a literal of that character.
        other => {
            out.push_str(&regex::escape(&other.to_string()));
            Ok(0)
        }
    }
}

/// Lower a BARE character `c` under the current magic level: emit the Rust operator when `c` is special
/// at this level, else a Rust-escaped literal.
fn lower_bare(c: char, magic: Magic, out: &mut String, groups: &mut usize) {
    // Is `c` an operator bare at this magic level? (Rust/regex is "very magic": all of these are
    // operators bare, so at `\v` we pass them through; at lower levels progressively fewer are.)
    let operator = match magic {
        Magic::Very => matches!(
            c,
            '(' | ')' | '{' | '}' | '+' | '?' | '|' | '.' | '*' | '[' | ']' | '^' | '$'
        ),
        Magic::Magic => matches!(c, '.' | '*' | '[' | ']' | '^' | '$'),
        Magic::NoMagic => matches!(c, '^' | '$'),
        Magic::VeryNoMagic => false, // only `\` is special; every bare char is literal
    };
    if operator {
        if c == '(' {
            *groups += 1;
        }
        // `\v` word boundaries `<`/`>` are handled in lower_escape's peer; here only the pass-through
        // operators land, which are already valid Rust syntax.
        out.push(c);
    } else {
        out.push_str(&regex::escape(&c.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pat: &str, hay: &str) -> Option<(usize, usize)> {
        let re = Regex::compile(pat, Options::default()).expect("compiles");
        re.find_at(hay, 0).map(|m| (m.start, m.end))
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
        assert!(matches!(
            Regex::compile("\\%(ab\\)", Options::default()),
            Err(RegexError::Unsupported(_))
        ));
    }
}
