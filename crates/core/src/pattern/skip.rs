//! Where a delimited pattern ends (Vim's `skip_regexp`).

use super::skip_anyof;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
