//! The substitute/global (`:s` / `:g`) data types and the two pure text helpers behind them
//! (`line_spans`, `expand_replacement`). The `EditorState::{substitute, substitute_preview,
//! apply_substitutions, global}` METHODS stay in `mod.rs`'s hub (they read private `View`/`Document`
//! state); they reach these helpers through `mod.rs`'s `pub(crate) use substitute::{…}` re-export.
//! The public types stay `pub` and remain reachable at the crate root via `crate::lib`'s re-export.

/// The pure decision for one command.
#[must_use]
/// The line range a `:s` acts on. `Lines` is 1-based inclusive (as the user types `:N,Ms`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubRange {
    /// No range prefix: only the cursor's line.
    CurrentLine,
    /// `:%s` — every line.
    WholeFile,
    /// `:N,Ms` — 1-based inclusive line numbers.
    Lines(usize, usize),
}

/// The `:s///` flags this MVP honors (F-009 #2). `c` (confirm) is an interactive frontend loop, not a
/// field here; `'gdefault'` is applied by the caller (it inverts `g`) before constructing this.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SubFlags {
    /// `g`: replace ALL matches on each line. Default (no `g`) = the first match per line only.
    pub global: bool,
    /// Case override from a flag: `i` → `Some(true)` (ignore case), `I` → `Some(false)`
    /// (case-sensitive); `None` = use the search config.
    pub ignore_case: Option<bool>,
}

/// One pending substitution from [`crate::EditorState::substitute_preview`]: the byte span `[start, end)` to
/// replace and the (expanded) replacement bytes, plus the 0-based line it sits on. Absolute offsets into
/// the CURRENT buffer — valid only until the buffer is edited, so an interactive confirm loop collects
/// the ACCEPTED subset and applies it all at once ([`crate::EditorState::apply_substitutions`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Substitution {
    /// Byte offset of the reported match start (the `\zs`-adjusted start).
    pub start: usize,
    /// Byte offset of the reported match end.
    pub end: usize,
    /// The replacement bytes to write over `[start, end)`.
    pub replacement: Vec<u8>,
    /// 0-based line index the match sits on (for the "M lines" echo + cursor placement).
    pub line: usize,
}

/// What a `:s` did, for the status echo ("N substitutions on M lines").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SubOutcome {
    /// Total matches replaced.
    pub replacements: usize,
    /// Distinct lines that had at least one replacement.
    pub lines: usize,
}

/// The command a `:g/pat/cmd` runs on each marked line (F-009 #4). MVP supports the two most common
/// forms; `:normal`, `:m`/`:t`, `:p`, etc. are post-MVP.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GlobalCmd {
    /// `:g/pat/d` — delete the matched line.
    Delete,
    /// `:g/pat/s/pat2/rep/flags` — substitute on the matched line.
    Substitute {
        /// The substitute pattern (the `:s` pattern, independent of the `:g` selector).
        pattern: String,
        /// The substitute replacement.
        replacement: String,
        /// The substitute flags (`g`/`i`/`I`; `c` confirm is not offered inside `:g`).
        flags: SubFlags,
    },
}

/// The byte span `(start, end)` of every line (`end` excludes the `\n`). Always at least one line.
pub(crate) fn line_spans(hay: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in hay.bytes().enumerate() {
        if b == b'\n' {
            out.push((start, i));
            start = i + 1;
        }
    }
    out.push((start, hay.len())); // the final line (unterminated, or empty after a trailing newline)
    out
}

/// Expand a `:s` replacement: `&` / `\0` → the whole matched text; `\n` `\t` `\r` `\\` `\&` escapes.
/// Capture backreferences `\1`-`\9` are a documented follow-up — an unsupported `\<d>` keeps the digit
/// literal rather than erroring.
pub(crate) fn expand_replacement(replacement: &str, matched: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => out.extend_from_slice(matched.as_bytes()),
            '\\' => match chars.next() {
                Some('0') => out.extend_from_slice(matched.as_bytes()),
                Some('n') => out.push(b'\n'),
                Some('t') => out.push(b'\t'),
                Some('r') => out.push(b'\r'),
                Some('&') => out.push(b'&'),
                Some('\\') => out.push(b'\\'),
                Some(other) => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                }
                None => out.push(b'\\'),
            },
            _ => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}
