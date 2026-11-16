//! Case conversion for the case operators and the Emacs case commands, under Vim's or Emacs's
//! rules.
//!
//! Split out of `planner/mod.rs` (#1392); the items are as they were there.

use crate::WordCase;

/// Recase `bytes`, keeping any invalid UTF-8 byte as it is: a file with stray bytes recases its letters
/// instead of panicking (#706).
pub(super) fn recase_bytes(bytes: &[u8], case: WordCase) -> Vec<u8> {
    recase_bytes_with(bytes, case, recase)
}

/// [`recase_bytes`] under the case `rules` (Vim's or Emacs's).
fn recase_bytes_with(bytes: &[u8], case: WordCase, rules: fn(&str, WordCase) -> String) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.extend_from_slice(rules(chunk.valid(), case).as_bytes());
        out.extend_from_slice(chunk.invalid());
    }
    out
}

/// Recase `text` for a Vim case operator (`gU` `gu` `g~` `~` `g?`): nvim's tables, one character to one
/// (`ß` → `ẞ`), composing characters kept ([`crate::casemap::vim_recase`]). `Capitalize` is not a Vim
/// operator; it takes the Emacs rules.
pub(super) fn recase(text: &str, case: WordCase) -> String {
    use crate::casemap::{vim_recase, VimCase};
    match case {
        WordCase::Upcase => vim_recase(text, VimCase::Upper),
        WordCase::Downcase => vim_recase(text, VimCase::Lower),
        WordCase::Toggle => vim_recase(text, VimCase::Toggle),
        WordCase::Rot13 => vim_recase(text, VimCase::Rot13),
        WordCase::Capitalize => recase_emacs(text, case),
    }
}

/// Recase `text` for an Emacs case command (`M-u` `M-l` `M-c`, the region commands): Emacs 31.1's
/// casefiddle rules, special casing (`ß` → `SS`) and final sigma included
/// ([`crate::casemap::emacs_recase`]). `Toggle` / `Rot13` are not Emacs commands; they take the Vim rules.
pub(super) fn recase_emacs(text: &str, case: WordCase) -> String {
    use crate::casemap::{emacs_recase, EmacsCase};
    match case {
        WordCase::Upcase => emacs_recase(text, EmacsCase::Up),
        WordCase::Downcase => emacs_recase(text, EmacsCase::Down),
        WordCase::Capitalize => emacs_recase(text, EmacsCase::Capitalize),
        WordCase::Toggle | WordCase::Rot13 => recase(text, case),
    }
}

/// Recase `b[s..e]` with `case` by `rules` ([`recase`] for Vim, [`recase_emacs`]), or `None` when the
/// span is empty or not valid UTF-8 (a grapheme-boundary span always is). Shared by `EmacsCaseWord`
/// (forward-word span), `EmacsCaseRegion` (the region) and a Visual-block `CaseSelection` (each row).
pub(super) fn recase_span(
    b: &[u8],
    s: usize,
    e: usize,
    case: WordCase,
    rules: fn(&str, WordCase) -> String,
) -> Option<Vec<u8>> {
    if e <= s {
        return None;
    }
    // A byte that is not UTF-8 is kept as it is, the text around it recased (#706, #1243).
    Some(recase_bytes_with(&b[s..e], case, rules))
}
