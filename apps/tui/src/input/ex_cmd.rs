//! Ex command words (`:help :abbreviate-commands`, #609): the table-driven resolver that turns a typed
//! command word — any abbreviation Neovim accepts — into the one spelling the ex parsers match, and the
//! command modifiers (`:silent`, `:verbose`, …) that may precede a command.
//!
//! Neovim resolves a word to the FIRST command in its table that it is a prefix of (`find_ex_command`), so
//! every command has a shortest accepted abbreviation and every longer prefix of its name resolves to it
//! too. [`COMMANDS`] records that shortest length for each command ruse implements, measured with
//! nvim v0.12.6's `fullcommand()`; a word shorter than it belongs to another command (`:s` is
//! `:substitute`, `:sig` is `:sign`), which ruse does not implement, so it stays unknown (E492).

use std::borrow::Cow;

/// One implemented command: its full Vim name, the shortest abbreviation nvim accepts for it, and the
/// spelling the ex parsers in `ex.rs` match.
struct ExCommand {
    name: &'static str,
    min: usize,
    canonical: &'static str,
}

const fn cmd(name: &'static str, min: usize, canonical: &'static str) -> ExCommand {
    ExCommand {
        name,
        min,
        canonical,
    }
}

/// The implemented Vim commands with nvim v0.12.6's minimal abbreviations (`fullcommand()` probe).
const COMMANDS: &[ExCommand] = &[
    cmd("write", 1, "w"),
    cmd("quit", 1, "q"),
    cmd("wq", 2, "wq"),
    cmd("xit", 1, "x"),
    cmd("exit", 3, "exit"),
    cmd("qall", 2, "qa"),
    cmd("update", 2, "update"),
    cmd("saveas", 3, "saveas"),
    cmd("wall", 2, "wall"),
    cmd("wqall", 3, "wqall"),
    cmd("xall", 2, "xall"),
    cmd("quitall", 5, "qa"),
    cmd("hide", 3, "hide"),
    cmd("split", 2, "split"),
    cmd("vsplit", 2, "vsplit"),
    cmd("close", 3, "close"),
    cmd("only", 2, "only"),
    cmd("terminal", 2, "terminal"),
    cmd("registers", 3, "registers"),
    cmd("display", 2, "display"),
    cmd("digraphs", 3, "digraphs"),
    cmd("marks", 5, "marks"),
    cmd("jumps", 2, "jumps"),
    cmd("changes", 7, "changes"),
    cmd("ascii", 2, "ascii"),
    cmd("nohlsearch", 3, "nohlsearch"),
    cmd("checkhealth", 3, "checkhealth"),
    cmd("checktime", 6, "checktime"),
    cmd("edit", 1, "edit"),
    cmd("enew", 3, "enew"),
    cmd("ls", 2, "ls"),
    cmd("buffers", 7, "buffers"),
    cmd("files", 5, "files"),
    cmd("bnext", 2, "bnext"),
    cmd("bNext", 2, "bprevious"),
    cmd("bprevious", 2, "bprevious"),
    cmd("bdelete", 2, "bdelete"),
    cmd("buffer", 1, "buffer"),
    cmd("lmap", 2, "lmap"),
    cmd("lunmap", 2, "lunmap"),
    cmd("cquit", 2, "cquit"),
    cmd("read", 1, "read"),
    cmd("delete", 1, "delete"),
    cmd("yank", 1, "yank"),
    cmd("join", 1, "join"),
    cmd("move", 1, "move"),
    cmd("copy", 2, "copy"),
    cmd("t", 1, "t"),
    cmd("put", 2, "put"),
    cmd("sort", 3, "sort"),
    cmd("set", 2, "set"),
    cmd("setlocal", 4, "setlocal"),
    cmd("setglobal", 4, "setglobal"),
    cmd("options", 2, "options"),
    cmd("earlier", 2, "earlier"),
    cmd("later", 3, "later"),
    cmd("substitute", 1, "s"),
    cmd("global", 1, "g"),
    cmd("vglobal", 1, "v"),
    cmd("normal", 4, "normal"),
    cmd("trust", 3, "trust"),
    // #609: the print family, `:retab` and the Insert starters.
    cmd("print", 1, "print"),
    cmd("list", 1, "list"),
    cmd("number", 2, "number"),
    cmd("z", 1, "z"),
    cmd("retab", 3, "retab"),
    cmd("startinsert", 4, "startinsert"),
    cmd("startreplace", 6, "startreplace"),
    cmd("startgreplace", 6, "startgreplace"),
    cmd("stopinsert", 5, "stopinsert"),
    // #610: the working directory and the file name.
    cmd("pwd", 2, "pwd"),
    cmd("cd", 2, "cd"),
    cmd("chdir", 3, "cd"),
    cmd("file", 1, "file"),
    cmd("find", 3, "find"),
    cmd("sfind", 2, "sfind"),
    cmd("drop", 2, "drop"),
    cmd("visual", 2, "visual"),
];

/// ruse's own commands (no Vim counterpart). Matched by exact spelling and never abbreviated, so they
/// cannot shadow a Vim abbreviation: none of these is a word nvim resolves (`:ca` is `:cabbrev` in nvim,
/// so it is not an alias of `:codeaction`).
const RUSE_COMMANDS: &[&str] = &[
    "fmt",
    "format",
    "references",
    "refs",
    "ref",
    "codeaction",
    "codeactions",
    "diagnostics",
    "diags",
    "diag",
    "rename",
    "rn",
    "trace",
    "untrust",
    "inspect",
];

/// A command modifier (`:help :command-modifiers`) ruse honours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Modifier {
    Silent,
    Unsilent,
    /// `:verbose`: `:set opt?` also says which config file set the value (#614).
    Verbose,
    /// Accepted with no effect: ruse has no autocommands.
    NoAutocmd,
    KeepJumps,
    KeepPatterns,
    KeepAlt,
    /// Accepted with no effect (#1234): in Vim `:keepmarks` only changes how a filter (`:{range}!`)
    /// adjusts marks; ruse does not honour it there yet.
    KeepMarks,
    Filter,
}

/// The modifiers with nvim v0.12.6's minimal abbreviations.
const MODIFIERS: &[(&str, usize, Modifier)] = &[
    ("silent", 3, Modifier::Silent),
    ("unsilent", 3, Modifier::Unsilent),
    ("verbose", 4, Modifier::Verbose),
    ("noautocmd", 3, Modifier::NoAutocmd),
    ("keepjumps", 5, Modifier::KeepJumps),
    ("keeppatterns", 5, Modifier::KeepPatterns),
    ("keepalt", 5, Modifier::KeepAlt),
    ("keepmarks", 3, Modifier::KeepMarks),
    ("filter", 4, Modifier::Filter),
];

/// What the modifiers before a command asked for.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ExModifiers {
    /// `:silent` — the command's normal messages are not shown.
    pub silent: bool,
    /// `:silent!` — its error messages are not shown either, and an error does not end the `|` chain.
    pub silent_errors: bool,
    /// `:verb[ose]` — `:set opt?` adds the `Last set from` line (`:help :set-verbose`).
    pub verbose: bool,
    /// `:keepj[umps]` — the command adds nothing to the jumplist or the change list.
    pub keep_jumps: bool,
    /// `:keepp[atterns]` — the command leaves the last search and substitute patterns as they were.
    pub keep_patterns: bool,
    /// `:keepa[lt]` — the command leaves the alternate file as it was.
    pub keep_alt: bool,
    /// `:filt[er][!] {pat}` — the command lists only the lines that match `pat` (`!`: that do not).
    pub filter: Option<(String, bool)>,
}

/// Every command name `:` completion offers (`:help cmdline-completion`, #613): the implemented Vim
/// commands' full names, ruse's own commands and the modifiers, unsorted.
pub(crate) fn command_names() -> impl Iterator<Item = &'static str> {
    COMMANDS
        .iter()
        .map(|c| c.name)
        .chain(RUSE_COMMANDS.iter().copied())
        .chain(MODIFIERS.iter().map(|m| m.0))
}

/// The full name of the command `word` abbreviates (an implemented Vim command or one of ruse's own),
/// for completing its argument.
pub(crate) fn full_command_name(word: &str) -> Option<&'static str> {
    RUSE_COMMANDS
        .iter()
        .copied()
        .find(|&c| c == word)
        .or_else(|| lookup(word).map(|c| c.name))
}

/// Whether `word` is a command modifier (`:sil`, `:verbose`, …).
pub(crate) fn is_modifier(word: &str) -> bool {
    MODIFIERS
        .iter()
        .any(|(name, min, _)| word.len() >= *min && name.starts_with(word))
}

/// The length of the leading range (`:help :range`) of an ex line: numbers, `.`, `$`, `%`, `,`, `;`,
/// `+`/`-` offsets, `'x` marks, `/pat/` and `?pat?` searches, and the blanks between them.
pub(crate) fn range_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'0'..=b'9' | b',' | b';' | b'%' | b'.' | b'$' | b'+' | b'-' | b' ' | b'\t' => i += 1,
            b'\'' if i + 1 < b.len() => {
                i += 1 + s[i + 1..].chars().next().map_or(1, char::len_utf8)
            }
            d @ (b'/' | b'?') => {
                // A search address runs to the next unescaped delimiter.
                let mut j = i + 1;
                while j < b.len() && b[j] != d {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                i = (j + 1).min(b.len());
            }
            _ => break,
        }
    }
    i
}

/// Vim's `:s` exception (`find_ex_command`): `s` directly followed by `c`, `g`, `i`, `I` or `r` is
/// `:substitute` with flags (`:sg`, `:sI`), except where that spells another command — `:sc[riptnames]`,
/// `:scs[cope]`, `:sre[wind]`, `:sim[alt]`, `:sig[n]`, `:sil[ent]`.
fn is_substitute_with_flags(word: &[u8]) -> bool {
    let at = |i: usize| word.get(i).copied().unwrap_or(0);
    at(0) == b's'
        && match at(1) {
            b'c' => {
                at(2) == 0
                    || (at(2) != b's'
                        && at(2) != b'r'
                        && (at(3) == 0 || (at(3) != b'i' && at(4) != b'p')))
            }
            b'g' | b'I' => true,
            b'i' => !matches!(at(2), b'm' | b'l' | b'g'),
            b'r' => at(2) != b'e',
            _ => false,
        }
}

/// The full name of the command `word` abbreviates, among the implemented ones, with its canonical
/// spelling.
fn lookup(word: &str) -> Option<&'static ExCommand> {
    COMMANDS
        .iter()
        .find(|c| word.len() >= c.min && c.name.starts_with(word))
}

/// The command word of `line` after its range: the run of letters, or `None` for a non-letter command
/// (`:!`, `:&`, `:<`, `:>`, `:=`, …) or an empty line. Returns the byte span of the word.
fn command_word(line: &str) -> Option<(usize, usize)> {
    let start = range_len(line);
    let len = line[start..]
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(line.len() - start);
    (len > 0).then_some((start, start + len))
}

/// Rewrite the command word of `line` to the spelling the ex parsers match: `:wr` → `:w`, `:del` →
/// `:delete`, `:su/a/b/` → `:s/a/b/`, `:norm x` → `:normal x`. A word that is no implemented command's
/// abbreviation, one of ruse's own commands, or the `:s{flags}` form is left as typed.
#[must_use]
pub fn resolve_command_word(line: &str) -> Cow<'_, str> {
    let Some((start, end)) = command_word(line) else {
        return Cow::Borrowed(line);
    };
    let word = &line[start..end];
    if RUSE_COMMANDS.contains(&word) || is_substitute_with_flags(word.as_bytes()) {
        return Cow::Borrowed(line);
    }
    match lookup(word) {
        Some(c) if c.canonical != word => {
            Cow::Owned(format!("{}{}{}", &line[..start], c.canonical, &line[end..]))
        }
        _ => Cow::Borrowed(line),
    }
}

/// Strip the command modifiers before an ex command (`:help :command-modifiers`): `:sil[ent][!]`,
/// `:uns[ilent]`, `:verb[ose]`, `:noa[utocmd]`, each followed by blanks (or `:`), in any order. As in
/// Vim, the modifiers come before the range (`:silent 2d`). Returns what they ask for and the rest.
#[must_use]
pub fn strip_modifiers(line: &str) -> (ExModifiers, &str) {
    let mut mods = ExModifiers::default();
    let mut rest = line.trim_start_matches([' ', '\t', ':']);
    loop {
        let len = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let word = &rest[..len];
        let Some(&(_, _, m)) = MODIFIERS
            .iter()
            .find(|(name, min, _)| word.len() >= *min && name.starts_with(word))
        else {
            return (mods, rest);
        };
        let mut after = &rest[len..];
        let bang = matches!(m, Modifier::Silent | Modifier::Filter) && after.starts_with('!');
        if bang {
            after = &after[1..];
        }
        // A modifier is a separate word: `:silentx` is not `:silent x`.
        if !after.is_empty() && !after.starts_with([' ', '\t', ':']) {
            return (mods, rest);
        }
        match m {
            Modifier::Silent => {
                mods.silent = true;
                mods.silent_errors |= bang;
            }
            Modifier::Unsilent => {
                mods.silent = false;
                mods.silent_errors = false;
            }
            Modifier::Verbose => mods.verbose = true,
            Modifier::NoAutocmd | Modifier::KeepMarks => {}
            Modifier::KeepJumps => mods.keep_jumps = true,
            Modifier::KeepPatterns => mods.keep_patterns = true,
            Modifier::KeepAlt => mods.keep_alt = true,
            Modifier::Filter => {
                let (pattern, next) = filter_pattern(after.trim_start());
                mods.filter = Some((pattern, bang));
                after = next;
            }
        }
        rest = after.trim_start_matches([' ', '\t', ':']);
    }
}

/// The pattern of a `:filter` (`:help :filter`): `/pat/` delimited by any character that is not a letter,
/// digit, `"` or `|` (it ends where an Ex pattern ends), or else the word up to the next blank. Returns it
/// and the text after it.
fn filter_pattern(s: &str) -> (String, &str) {
    match s.chars().next() {
        Some(d) if !d.is_ascii_alphanumeric() && !matches!(d, '"' | '|' | ' ' | '\t') => {
            let body = &s[d.len_utf8()..];
            let end = ruse_core::pattern::skip_regexp(body, d, true);
            let rest = body[end..].strip_prefix(d).unwrap_or(&body[end..]);
            (body[..end].to_string(), rest)
        }
        _ => {
            let end = s.find([' ', '\t']).unwrap_or(s.len());
            (s[..end].to_string(), &s[end..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviations_resolve_like_nvim() {
        for (typed, want) in [
            ("wr", "w"),
            ("write", "w"),
            ("del", "delete"),
            ("d", "delete"),
            ("clos", "close"),
            ("ju", "jumps"),
            ("bu 2", "buffer 2"),
            ("b2", "buffer2"),
            ("su/a/b/", "s/a/b/"),
            ("2,3subs/a/b/g", "2,3s/a/b/g"),
            ("norm x", "normal x"),
            ("norma! x", "normal! x"),
            ("bN", "bprevious"),
            ("e!", "edit!"),
            ("q!", "q!"),
            ("quita!", "qa!"),
            ("glo/a/d", "g/a/d"),
            ("mo0", "move0"),
            ("co.", "copy."),
            ("'a,'bd", "'a,'bdelete"),
        ] {
            assert_eq!(resolve_command_word(typed), want, ":{typed}");
        }
    }

    #[test]
    fn other_commands_words_stay_as_typed() {
        // Too short for an implemented command (nvim resolves them elsewhere), ruse's own commands, the
        // `:s{flags}` form, and non-letter commands.
        for typed in [
            "sig", "sil", "ca", "no", "nor", "cl", "mar", "sg", "sI", "sc", "sr", "fmt", "diag",
            "rn x", "!ls", "&&", ">>", "", "normalx",
        ] {
            assert_eq!(resolve_command_word(typed), typed, ":{typed}");
        }
    }

    #[test]
    fn modifiers_strip_before_the_range() {
        assert_eq!(
            strip_modifiers("silent 2d"),
            (
                ExModifiers {
                    silent: true,
                    ..ExModifiers::default()
                },
                "2d"
            )
        );
        assert_eq!(
            strip_modifiers("sil! verb noa s/a/b/"),
            (
                ExModifiers {
                    silent: true,
                    silent_errors: true,
                    verbose: true,
                    ..ExModifiers::default()
                },
                "s/a/b/"
            )
        );
        assert_eq!(
            strip_modifiers("silent unsilent d"),
            (ExModifiers::default(), "d")
        );
        assert_eq!(strip_modifiers("si d").1, "si d");
        assert_eq!(strip_modifiers("silentx").1, "silentx");
        assert_eq!(strip_modifiers("d").1, "d");
        let (mods, rest) = strip_modifiers("keepj keepp keepa 2d");
        assert!(mods.keep_jumps && mods.keep_patterns && mods.keep_alt);
        assert_eq!(rest, "2d");
        // Too short for `:keepjumps`, so nvim reads `:keep` as `:keepmarks` (`fullcommand('keep')`).
        let (mods, rest) = strip_modifiers("keep d");
        assert!(!mods.keep_jumps);
        assert_eq!(rest, "d");
        let (mods, rest) = strip_modifiers("filter /a b/ %p");
        assert_eq!(mods.filter, Some(("a b".to_string(), false)));
        assert_eq!(rest, "%p");
        let (mods, rest) = strip_modifiers("filt! foo nu");
        assert_eq!(mods.filter, Some(("foo".to_string(), true)));
        assert_eq!(rest, "nu");
    }
}
