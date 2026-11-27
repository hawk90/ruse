//! Collections `[...]`: reading their elements and emitting the Rust class set.

use super::bytes::ESCAPE_BASE;
use super::classes::{F_BODY, I_BODY, K_BODY, P_BODY};
use super::lex::{ABBR, IN_RANGE};
use super::RegexError;

/// Skip over a `[]` collection body starting just after the `[` (Vim's `skip_anyof`). Returns the index
/// of the closing `]`, or `s.len()` if there is none.
pub(super) fn skip_anyof(s: &[char], mut p: usize) -> usize {
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
pub(super) fn equi_class_at(s: &[char], p: usize) -> Option<usize> {
    (s.get(p + 1) == Some(&'=')
        && s.get(p + 2).is_some()
        && s.get(p + 3) == Some(&'=')
        && s.get(p + 4) == Some(&']'))
    .then_some(5)
}

/// `[.a.]` at `p`: the character and the length.
pub(super) fn coll_element_at(s: &[char], p: usize) -> Option<(char, usize)> {
    (s.get(p + 1) == Some(&'.')
        && s.get(p + 2).is_some()
        && s.get(p + 3) == Some(&'.')
        && s.get(p + 4) == Some(&']'))
    .then(|| (s[p + 2], 5))
}

/// `[:lower:]`: the lowercase letters, and the escape chars of the bytes whose Latin-1 char nvim calls
/// lowercase (`utf_islower`: µ, ß, à-ö, ø-ÿ), since nvim tests a byte that is not UTF-8 as that char (#1269).
const LOWER_BODY: &str = r"\p{Ll}\x{10FFB5}\x{10FFDF}-\x{10FFF6}\x{10FFF8}-\x{10FFFF}";
/// `[:upper:]`: the uppercase letters, and the escape chars of the bytes whose Latin-1 char is uppercase
/// (À-Ö, Ø-Þ, #1269).
const UPPER_BODY: &str = r"\p{Lu}\x{10FFC0}-\x{10FFD6}\x{10FFD8}-\x{10FFDE}";

/// A `[:name:]` class: its Rust class body (valid inside `[...]`) and whether it must stay exact under
/// `'ignorecase'`. Vim tests a named class without case folding (`check_char_class`), so `[:upper:]`
/// does not match `a`, and no class that holds letters may pick up Rust's Unicode folds (KELVIN SIGN
/// U+212A for `k`, LONG S U+017F for `s`, ...). Letter-free classes fold to themselves either way.
#[derive(Clone, Copy)]
pub(super) struct NamedClass {
    body: &'static str,
    case_sensitive: bool,
}

/// `[:name:]` at `p`: the class and the length of the item.
pub(super) fn char_class_at(s: &[char], p: usize) -> Option<(NamedClass, usize)> {
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
        ("lower", LOWER_BODY, true),
        ("print", P_BODY, true),
        (
            "punct",
            r"\x{21}-\x{2F}\x{3A}-\x{40}\x{5B}-\x{60}\x{7B}-\x{7E}",
            false,
        ),
        ("return", r"\r", false),
        ("space", r"\t\x{B}-\x{D}\x{20}", false),
        ("tab", r"\t", false),
        ("upper", UPPER_BODY, true),
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
pub(super) struct CollSet {
    /// Pieces folded by `'ignorecase'` like any literal.
    items: String,
    /// `[:upper:]` / `[:lower:]`, which `'ignorecase'` does not fold.
    exact: String,
    /// A `\n` item: the collection also matches a line break.
    pub(super) newline: bool,
    /// Some item covers U+000A, which a non-`\_` collection must not match (Vim never matches a line
    /// break that way).
    covers_lf: bool,
    /// The character and range items as code-point ranges, for closing the set under 'ignorecase'.
    ranges: Vec<(u32, u32)>,
}

impl CollSet {
    pub(super) fn push_char(&mut self, c: char) {
        self.covers_lf |= c == '\n';
        self.items.push_str(&class_char(c));
        self.ranges.push((u32::from(c), u32::from(c)));
    }

    pub(super) fn push_range(&mut self, lo: char, hi: char) {
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
        let mut folded = Vec::new();
        for class in crate::casemap::vim_fold_classes() {
            if class.iter().any(|&m| inside(m)) {
                for &m in class.iter().filter(|&&m| !inside(m)) {
                    extra.push_str(&format!(r"\x{{{m:X}}}"));
                    folded.push((m, m));
                }
            }
        }
        self.items.push_str(&extra);
        self.ranges.extend(folded);
    }

    /// nvim compares a byte that is not UTF-8 with a collection's items as the Latin-1 char of its value
    /// (`[ð-ÿ]` and `[ÿ]` take byte 0xFF, #1252), so every item in U+0080-U+00FF also takes the escape char
    /// that spells that byte ([`escape_char`](super::escape_char)).
    fn add_latin1_bytes(&mut self) {
        let mut extra = String::new();
        for &(lo, hi) in &self.ranges {
            let (lo, hi) = (lo.max(0x80), hi.min(0xFF));
            if lo <= hi {
                let (a, b) = (lo + ESCAPE_BASE, hi + ESCAPE_BASE);
                extra.push_str(&format!(r"\x{{{a:X}}}-\x{{{b:X}}}"));
            }
        }
        self.items.push_str(&extra);
    }

    pub(super) fn push_class(&mut self, class: NamedClass) {
        if class.case_sensitive {
            self.exact.push_str(class.body);
        } else {
            self.items.push_str(class.body);
        }
    }

    pub(super) fn emit(
        mut self,
        negated: bool,
        extra: bool,
        ci: bool,
    ) -> Result<String, RegexError> {
        if ci {
            self.fold_items();
        }
        self.add_latin1_bytes();
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
