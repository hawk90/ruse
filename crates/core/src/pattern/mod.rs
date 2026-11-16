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

use crate::pos::BytePos;

mod bytes;
mod classes;
mod collection;
mod lex;
mod parse;
mod skip;

pub(crate) use bytes::escaped_byte;
pub use bytes::{spell, unspell, Hay};
pub use skip::skip_regexp;

use collection::skip_anyof;
use parse::lower;

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
    pub start: BytePos,
    /// Byte offset of the (possibly `\ze`-adjusted) match end.
    pub end: BytePos,
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
    /// The pattern opens with `\<`, dropped from `inner` and checked on each candidate's start with
    /// Vim's class rule instead (#745, [`crate::charclass::at_word_start`]).
    bow: bool,
    /// The pattern closes with `\>`, dropped from `inner` and checked on each candidate's end
    /// ([`crate::charclass::at_word_end`]).
    eow: bool,
    /// For a closing `\>` on a pattern whose end can vary and that holds no other zero-width
    /// assertion: `^(?:inner)\z`, run on a slice `[start, end)` to try the other ends at a start whose
    /// first end failed the `\>` check. Without assertions a slice match is exact, since nothing in the
    /// pattern looks outside the slice.
    retry: Option<RawRegex>,
}

/// A candidate match: the whole span (before `\zs`/`\ze`) and the reported one.
#[derive(Clone, Copy)]
struct Hit {
    whole: (usize, usize),
    rep: Match,
}

/// How many other ends a failed `\>` check tries at one start, and how far (in characters) it looks
/// for them on each side of the first end. Past these bounds the start is given up (a documented gap).
const RETRY_ENDS: usize = 64;
const RETRY_SCAN: usize = 1024;

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
        let build = |src: &str| {
            regex::RegexBuilder::new(src)
                .build()
                .map_err(|e| RegexError::Syntax(e.to_string()))
        };
        let inner = build(&low.rust)?;
        let retry = if low.eow && low.variable && low.asserts == 0 {
            Some(build(&format!(r"^(?:{})\z", low.rust))?)
        } else {
            None
        };
        Ok(Regex {
            inner,
            zs: low.zs,
            ze: low.ze,
            multiline: low.has_nl,
            bow: low.bow,
            eow: low.eow,
            retry,
        })
    }

    /// The first match in `hay` and its Vim groups, `\0` (the whole match) then `\1`..: each group's byte
    /// span, `None` for one that did not take part. For a caller that expands `\N` itself (Emacs's
    /// `find-sibling-rules`, #619). The `\zs`/`\ze` markers are not groups; `\<`/`\>` are not checked.
    #[must_use]
    pub fn groups(&self, hay: &str) -> Option<Vec<Option<(usize, usize)>>> {
        let caps = self.inner.captures(hay)?;
        let span = |i: usize| caps.get(i).map(|m| (m.start(), m.end()));
        let mut out = vec![span(0)];
        for i in 1..caps.len() {
            if !self.zs.contains(&i) && !self.ze.contains(&i) {
                out.push(span(i));
            }
        }
        Some(out)
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
    pub fn find_whole_at(&self, hay: &str, from: BytePos) -> Option<(BytePos, Match)> {
        let hit = self.next(hay, from.0)?;
        if phantom_line(hay, hit) {
            return None;
        }
        Some((BytePos(hit.whole.0), hit.rep))
    }

    /// The first match at a byte offset `>= from`, honoring the `\zs`/`\ze` reported span. `None` if
    /// there is no match from `from` to the end (the caller wraps for `/`-search).
    #[must_use]
    pub fn find_at(&self, hay: &str, from: BytePos) -> Option<Match> {
        let hit = self.next(hay, from.0)?;
        if phantom_line(hay, hit) {
            return None; // the only thing left to match is the slot past the last line
        }
        Some(hit.rep)
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
                let Some(hit) = self.next(hay, col) else {
                    return out; // no match anywhere further down
                };
                if phantom_line(hay, hit) {
                    return out;
                }
                let whole = hit.whole.0;
                if whole > le {
                    // The leftmost match starts on a later line: no line before it has one.
                    next = hay[..whole].rfind('\n').map_or(0, |i| i + 1);
                    break;
                }
                let m = hit.rep;
                let step = |at: usize| at + hay[at..].chars().next().map_or(1, char::len_utf8);
                if substitute && prev_end == Some(col) && m.start.0 == m.end.0 && m.end.0 == col {
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
                if m.end.0 > le {
                    break; // it ran onto a later line
                }
                if substitute {
                    col = m.end.0;
                    prev_end = Some(col);
                    if col >= le && !self.multiline {
                        break;
                    }
                } else if m.start == m.end {
                    if m.end.0 >= le {
                        break;
                    }
                    col = step(m.end.0);
                } else {
                    col = m.end.0;
                }
            }
            if next <= ls {
                break; // cannot happen (a match past `le` is on a later line); never loop
            }
            ls = next;
        }
        out
    }

    /// The first candidate at a byte offset `>= from` whose edges pass Vim's `\<` / `\>` checks (#745).
    /// A start that fails `\<` cannot match at all, and one whose end fails `\>` tries its other ends
    /// ([`Regex::other_end`]); either way the search goes on from the next character.
    fn next(&self, hay: &str, from: usize) -> Option<Hit> {
        let mut at = from.min(hay.len());
        if !self.bow && !self.eow {
            let caps = self.inner.captures_at(hay, at)?;
            return Some(self.hit(&caps, 0));
        }
        loop {
            let m = self.inner.find_at(hay, at)?;
            let (s, e) = (m.start(), m.end());
            if !self.bow || crate::charclass::at_word_start(hay, s) {
                if !self.eow || crate::charclass::at_word_end(hay, e) {
                    return Some(self.resolve(hay, s, e));
                }
                if let Some(hit) = self.other_end(hay, s, e) {
                    return Some(hit);
                }
            }
            at = s + hay[s..].chars().next()?.len_utf8();
        }
    }

    /// The match at `[s, e)` found by `inner`, with its reported span.
    fn resolve(&self, hay: &str, s: usize, e: usize) -> Hit {
        if self.zs.is_empty() && self.ze.is_empty() {
            return Hit {
                whole: (s, e),
                rep: Match {
                    start: BytePos(s),
                    end: BytePos(e),
                },
            };
        }
        // The match at a start does not depend on where the search began, so this is the same one.
        match self.inner.captures_at(hay, s) {
            Some(caps) => self.hit(&caps, 0),
            None => Hit {
                whole: (s, e),
                rep: Match {
                    start: BytePos(s),
                    end: BytePos(e),
                },
            },
        }
    }

    /// At a start `s` whose preferred end `e` failed the closing `\>`, another end that passes it and
    /// that the pattern matches exactly from `s`. Vim's engine would take the highest-priority such
    /// path; ruse cannot rank paths, so it tries the nearer shorter ends first (right for a greedy
    /// tail, `.*\>`), then the longer ones (right for a lazy one, `.\{-}\>`). Only patterns with a
    /// variable end and no other zero-width assertion get here (`retry`).
    fn other_end(&self, hay: &str, s: usize, e: usize) -> Option<Hit> {
        let retry = self.retry.as_ref()?;
        let limit = if self.multiline {
            hay.len()
        } else {
            hay[e..].find('\n').map_or(hay.len(), |i| e + i)
        };
        let mut tries = 0;
        let mut attempt = |end: usize| -> Option<Hit> {
            if !crate::charclass::at_word_end(hay, end) || tries >= RETRY_ENDS {
                return None;
            }
            tries += 1;
            let caps = retry.captures(&hay[s..end])?;
            Some(self.hit(&caps, s))
        };
        let shorter = hay[s..e]
            .char_indices()
            .rev()
            .map(|(i, _)| s + i)
            .take(RETRY_SCAN);
        for end in shorter {
            if let Some(hit) = attempt(end) {
                return Some(hit);
            }
        }
        let longer = hay[e..limit]
            .char_indices()
            .skip(1)
            .map(|(i, _)| e + i)
            .chain(std::iter::once(limit).filter(|&l| l > e))
            .take(RETRY_SCAN);
        for end in longer {
            if let Some(hit) = attempt(end) {
                return Some(hit);
            }
        }
        None
    }

    /// A capture set (offsets relative to `base`) as a candidate.
    fn hit(&self, caps: &regex::Captures, base: usize) -> Hit {
        let whole = caps.get(0).expect("group 0 always present");
        let rep = self.report(caps);
        Hit {
            whole: (base + whole.start(), base + whole.end()),
            rep: Match {
                start: BytePos(base + rep.start.0),
                end: BytePos(base + rep.end.0),
            },
        }
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
        Match {
            start: BytePos(start),
            end: BytePos(end),
        }
    }
}

/// Is this match the empty slot after a haystack's final `\n`? A whole-buffer haystack ends in "\n", and
/// the position after it is not a line in Vim: `^`, `$`, `\_$`, `x*` must not match there (nvim:
/// `/^$` on "abc\ndef\n" is E486, and `/^` wraps instead of landing past the end). A match that STARTS
/// there can only be zero-width, so dropping it never hides a real match. A haystack that does not end
/// in "\n" (one line, an empty buffer) is unaffected.
fn phantom_line(hay: &str, hit: Hit) -> bool {
    !hay.is_empty() && hit.whole.0 == hay.len() && hay.ends_with('\n')
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod group_tests {
    use super::*;

    /// #619: the Vim groups of a match, the `\\zs` marker skipped.
    #[test]
    fn groups_are_the_vim_groups() {
        let re = Regex::compile(r"\([a-z]\+\)\.\zs\(c\)$", Options::default()).unwrap();
        let g = re.groups("dir/foo.c").unwrap();
        assert_eq!(g, vec![Some((4, 9)), Some((4, 7)), Some((8, 9))]);
        assert!(re.groups("foo.h").is_none());
    }
}
