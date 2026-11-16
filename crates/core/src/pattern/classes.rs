//! The class atoms `\i \k \f \p …` and their Rust class bodies.

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
    r"\x{1F000}-\x{1F2FF}\x{1F300}-\x{1F9FF}\x{10FF80}-\x{10FFFF}",
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
            r"\x{2060}-\x{206F}\x{FEFF}\x{FFF9}-\x{FFFB}\x{FFFE}-\x{FFFF}\x{10FF80}-\x{10FF9F}]]",
        )
    };
}

// A byte that is not UTF-8 (an escape char, see [`Hay`]) is in a class when the Latin-1 char of its value
// is: the `*_ESC` bodies are the escape chars of each class's Latin-1 part (0x80-0xFF).
pub(super) const I_BODY: &str = r"0-9A-Za-z_\x{B5}\x{C0}-\x{FF}\x{10FFB5}\x{10FFC0}-\x{10FFFF}";
const I_NODIGIT: &str = r"A-Za-z_\x{B5}\x{C0}-\x{FF}\x{10FFB5}\x{10FFC0}-\x{10FFFF}";
pub(super) const K_BODY: &str = concat!(
    r"0-9A-Za-z_\x{B5}\x{C0}-\x{FF}\x{10FFB5}\x{10FFC0}-\x{10FFFF}",
    k_wide!()
);
const K_NODIGIT: &str = concat!(
    r"A-Za-z_\x{B5}\x{C0}-\x{FF}\x{10FFB5}\x{10FFC0}-\x{10FFFF}",
    k_wide!()
);
// `\f` and `\p` take 0xA0-0xFF of Latin-1, so every escape char from U+10FFA0 (in the wide range already).
pub(super) const F_BODY: &str =
    r"[\x{23}-\x{25}\x{2B}-\x{39}\x{3D}A-Z_a-z\x{7E}\x{A0}-\x{10FFFF}--\x{10FF80}-\x{10FF9F}]";
const F_NODIGIT: &str =
    r"[\x{23}-\x{25}\x{2B}-\x{2F}\x{3D}A-Z_a-z\x{7E}\x{A0}-\x{10FFFF}--\x{10FF80}-\x{10FF9F}]";
pub(super) const P_BODY: &str = concat!(r"\x{20}-\x{7E}\x{A0}-\x{FF}", p_wide!());
const P_NODIGIT: &str = concat!(r"\x{20}-\x{2F}\x{3A}-\x{7E}\x{A0}-\x{FF}", p_wide!());

/// A class atom (`.` or `\i \I … \u \U`), optionally with the `\_` newline form. Vim's classes are
/// ASCII (or Latin-1 for the option-driven ones); a negated class never matches a line break unless
/// it is the `\_` form. `\l \L \u \U` keep their case under `'ignorecase'`.
pub(super) fn class_atom(c: char, newline: bool, ci: bool) -> String {
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
pub(super) fn recognized_class(
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
