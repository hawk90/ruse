//! Case mapping and case folding the way the pinned editors do it (#912, #1112, #732).
//!
//! The Vim profile recases (`gU` `gu` `g~` `~` `g?`, Visual `U` `u` `~`) and folds for `'ignorecase'`
//! with Neovim's tables; the Emacs profile recases (`M-u` `M-l` `M-c`, the region commands) with Emacs's.
//! Rust's `char::to_uppercase` agrees with neither: nvim maps one character to one (`ß` → `ẞ`, never
//! `SS`) and keeps a titlecase letter under `gu`; Emacs applies the unconditional special casing
//! (`ß` → `SS`, `ﬁ` → `FI`) but its case table leaves `ı` and `ſ` alone. The tables live in the generated
//! `case_tables.rs`, captured from the binaries by `tools/parity/case_tables.py`.

use crate::case_tables as t;
use unicode_segmentation::UnicodeSegmentation;

fn pair(table: &[(u32, u32)], c: char) -> char {
    match table.binary_search_by_key(&u32::from(c), |&(k, _)| k) {
        Ok(i) => char::from_u32(table[i].1).unwrap_or(c),
        Err(_) => c,
    }
}

fn string_map(table: &'static [(u32, &'static str)], c: char) -> Option<&'static str> {
    table
        .binary_search_by_key(&u32::from(c), |&(k, _)| k)
        .ok()
        .map(|i| table[i].1)
}

/// nvim's `toupper()` of one character (`mb_toupper`, default `'casemap'`).
#[must_use]
pub fn vim_toupper(c: char) -> char {
    pair(t::NVIM_TOUPPER, c)
}

/// nvim's `tolower()` of one character (`mb_tolower`, default `'casemap'`).
#[must_use]
pub fn vim_tolower(c: char) -> char {
    pair(t::NVIM_TOLOWER, c)
}

/// The characters `'ignorecase'` matches with `c` (nvim's `utf_fold(a) == utf_fold(c)`), `c` included,
/// sorted; `None` when `c` matches only itself.
#[must_use]
pub fn vim_fold_class(c: char) -> Option<&'static [u32]> {
    t::NVIM_FOLD_INDEX
        .binary_search_by_key(&u32::from(c), |&(k, _)| k)
        .ok()
        .map(|i| t::NVIM_FOLD_CLASSES[usize::from(t::NVIM_FOLD_INDEX[i].1)])
}

/// Every fold class (two or more characters each) — for closing a collection under `'ignorecase'`.
pub(crate) fn vim_fold_classes() -> &'static [&'static [u32]] {
    t::NVIM_FOLD_CLASSES
}

/// A Vim case operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimCase {
    /// `gU`, Visual `U`.
    Upper,
    /// `gu`, Visual `u`.
    Lower,
    /// `g~`, `~`.
    Toggle,
    /// `g?`: ASCII letters only.
    Rot13,
}

/// nvim's `swapchar` on one character.
fn swapchar(op: VimCase, c: char) -> char {
    let rot = |c: char, base: u8| (((c as u8 - base + 13) % 26) + base) as char;
    if op == VimCase::Rot13 && !c.is_ascii() {
        return c;
    }
    let up = vim_toupper(c);
    if up != c {
        // `mb_islower`: it has an upper case (a titlecase letter like `ǅ` counts).
        return match op {
            VimCase::Rot13 => rot(c, b'a'),
            VimCase::Lower => c,
            VimCase::Upper | VimCase::Toggle => up,
        };
    }
    let low = vim_tolower(c);
    if low != c {
        return match op {
            VimCase::Rot13 => rot(c, b'A'),
            VimCase::Upper => c,
            VimCase::Lower | VimCase::Toggle => low,
        };
    }
    c
}

/// Recase `text` the way nvim's `op_tilde` does: each character cell's base character goes through
/// `swapchar`, one character to one, and its composing characters are kept as they are.
#[must_use]
pub fn vim_recase(text: &str, op: VimCase) -> String {
    let mut out = String::with_capacity(text.len());
    for g in text.graphemes(true) {
        let mut chars = g.chars();
        if let Some(base) = chars.next() {
            out.push(swapchar(op, base));
            out.push_str(chars.as_str());
        }
    }
    out
}

/// An Emacs case command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmacsCase {
    /// `upcase-word`, `upcase-region`.
    Up,
    /// `downcase-word`, `downcase-region`.
    Down,
    /// `capitalize-word`, `capitalize-region`.
    Capitalize,
}

/// Whether `c` has word syntax in Emacs's standard syntax table.
#[must_use]
pub fn emacs_is_word(c: char) -> bool {
    let c = u32::from(c);
    t::EMACS_WORD
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Recase `text` the way Emacs's `casify_region` does over exactly that span (casefiddle.c): every
/// character by the special casing and the standard case table, a word's first character to title case
/// under `Capitalize` (the word state starts outside a word at the span start), and a capital sigma
/// downcased at the end of a word becomes the final `ς`.
#[must_use]
pub fn emacs_recase(text: &str, case: EmacsCase) -> String {
    const SIGMA: char = '\u{3A3}';
    let mut out = String::with_capacity(text.len());
    let mut inword = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let was_inword = inword;
        inword = emacs_is_word(ch);
        let table = match case {
            EmacsCase::Up => t::EMACS_UPCASE,
            EmacsCase::Down => t::EMACS_DOWNCASE,
            EmacsCase::Capitalize if was_inword => t::EMACS_DOWNCASE,
            EmacsCase::Capitalize => t::EMACS_CAPITALIZE,
        };
        match string_map(table, ch) {
            Some(_)
                if ch == SIGMA && was_inword && chars.peek().is_none_or(|&n| !emacs_is_word(n)) =>
            {
                out.push('\u{3C2}');
            }
            Some(s) => out.push_str(s),
            None => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables come from the pinned editors; a repin (spec/parity/upstreams.yaml) regenerates them.
    #[test]
    fn tables_come_from_the_pinned_editors() {
        let pins = include_str!("../../../spec/parity/upstreams.yaml");
        let nvim = t::NVIM_VERSION.trim_start_matches("NVIM ");
        let emacs = t::EMACS_VERSION.trim_start_matches("GNU Emacs ");
        assert!(pins.contains(&format!("version_label: {nvim}")), "{nvim}");
        assert!(
            pins.contains(&format!("version_label: emacs-{emacs}")),
            "{emacs}"
        );
    }

    #[test]
    fn vim_maps_one_character_to_one() {
        assert_eq!(vim_recase("straße", VimCase::Upper), "STRAẞE");
        assert_eq!(vim_recase("ẞ", VimCase::Lower), "ß");
        assert_eq!(vim_recase("ﬁ ΐ", VimCase::Upper), "ﬁ ΐ");
        // A titlecase letter has an upper case, so `gu` keeps it and `g~` upcases it.
        assert_eq!(vim_recase("ǅ", VimCase::Lower), "ǅ");
        assert_eq!(vim_recase("ǅ", VimCase::Toggle), "Ǆ");
        assert_eq!(vim_recase("ı ſ İ", VimCase::Upper), "I S İ");
        assert_eq!(vim_recase("İ", VimCase::Lower), "i");
        assert_eq!(vim_recase("e\u{301}x", VimCase::Rot13), "r\u{301}k");
        assert_eq!(vim_recase("ΣΑΣ", VimCase::Lower), "σασ");
    }

    #[test]
    fn vim_fold_classes_follow_utf_fold() {
        assert_eq!(vim_fold_class('k'), Some(&[0x4B, 0x6B, 0x212A][..]));
        assert_eq!(vim_fold_class('ß'), None);
        assert_eq!(vim_fold_class('ẞ'), None);
        assert_eq!(vim_fold_class('\u{1F80}'), None);
        assert_eq!(vim_fold_class('1'), None);
    }

    #[test]
    fn emacs_special_casing_and_final_sigma() {
        assert_eq!(emacs_recase("straße", EmacsCase::Up), "STRASSE");
        assert_eq!(emacs_recase("ﬁsh", EmacsCase::Capitalize), "Fish");
        assert_eq!(emacs_recase("ßa", EmacsCase::Capitalize), "Ssa");
        assert_eq!(emacs_recase("ǆemal", EmacsCase::Capitalize), "ǅemal");
        assert_eq!(emacs_recase("ı ſ", EmacsCase::Up), "ı ſ");
        assert_eq!(emacs_recase("İx", EmacsCase::Down), "i\u{307}x");
        assert_eq!(emacs_recase("ΌΣΟΣ x", EmacsCase::Down), "όσος x");
        assert_eq!(emacs_recase("ΣΑ", EmacsCase::Down), "σα");
        assert_eq!(emacs_recase("ΑΣΑ", EmacsCase::Down), "ασα");
        assert_eq!(emacs_recase("ΌΣΟΣ", EmacsCase::Capitalize), "Όσος");
        assert_eq!(emacs_recase("foo_bar", EmacsCase::Capitalize), "Foo_Bar");
    }
}
