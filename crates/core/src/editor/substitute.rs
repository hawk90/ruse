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

/// A destination line address for `:m`/`:t` — where to place the moved/copied block. Resolved against the
/// buffer to a 0-based INSERT INDEX in `0..=line_count` (`Line(0)` = top; `Last` = end; `Current` = after
/// the cursor's line). Vim's `:m N` means "after line N", so `Line(n)` inserts before 0-based line `n`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineAddr {
    /// `:m N` / `:m 0` — after 1-based line N (0 = the very top). Stored as the insert index N.
    Line(usize),
    /// `:m $` — after the last line (the buffer end).
    Last,
    /// `:m .` — after the current (cursor) line.
    Current,
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

/// Resolve a [`SubRange`] to an inclusive 0-based line index pair `(first, last)` over `lines`, using
/// `cursor_line` for the no-range `CurrentLine` default (as Vim's `:d`/`:m`/`:t`/`:y` do). Both ends are
/// clamped to the last line; the caller guards the empty-buffer case.
pub(crate) fn resolve_line_range(
    range: SubRange,
    lines: &[(usize, usize)],
    cursor_line: usize,
) -> (usize, usize) {
    let last = lines.len().saturating_sub(1);
    match range {
        SubRange::CurrentLine => (cursor_line.min(last), cursor_line.min(last)),
        SubRange::WholeFile => (0, last),
        SubRange::Lines(a, b) => (a.saturating_sub(1).min(last), b.saturating_sub(1).min(last)),
    }
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

/// The active case transform for the replacement expander: uppercase or lowercase.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
}

/// The case-modifier state machine driving [`expand_replacement`]. Vim's `\u`/`\l` set a ONE-CHARACTER
/// pending case that takes priority over any region for the next emitted char, then reverts; `\U`/`\L`
/// open a region that persists until `\e`/`\E`. Setting a region does NOT clear a pending one-char
/// modifier (verified vs nvim: `\l\U&` on "abcd" → "aBCD"), and any emitted char — text OR a control
/// escape like `\r` — consumes the pending modifier (`\u\rx` leaves `x` lowercase).
#[derive(Default)]
struct CaseState {
    /// A one-shot `\u`/`\l` awaiting the next emitted character; wins over `region` for that char.
    pending: Option<Case>,
    /// A `\U`/`\L` region in effect until `\e`/`\E`.
    region: Option<Case>,
}

impl CaseState {
    /// Emit `s` into `out`, applying the pending one-char case to the FIRST char and the region case to
    /// the rest. The pending modifier is consumed by the first char (if any); an empty `s` leaves it.
    fn emit(&mut self, s: &str, out: &mut Vec<u8>) {
        let mut buf = [0u8; 4];
        for ch in s.chars() {
            match self.pending.take().or(self.region) {
                Some(Case::Upper) => {
                    for u in ch.to_uppercase() {
                        out.extend_from_slice(u.encode_utf8(&mut buf).as_bytes());
                    }
                }
                Some(Case::Lower) => {
                    for l in ch.to_lowercase() {
                        out.extend_from_slice(l.encode_utf8(&mut buf).as_bytes());
                    }
                }
                None => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
            }
        }
    }
}

/// Expand a `:s` replacement: `&` / `\0` → the whole matched text; `\n` `\t` `\r` `\\` `\&` escapes; and
/// the case modifiers `\u` `\l` (next char) / `\U` `\L` (region) / `\e` `\E` (end region), applied to the
/// emitted text (including `&`/`\0` insertions) as a small state machine — see [`CaseState`].
///
/// Capture backreferences `\1`-`\9` are DEFERRED: `\(…\)` compiles to a real regex group, but the
/// `pattern::Match` type surfaces only the reported `start`/`end` span, not per-group spans, and the
/// substitute path passes only the whole matched slice here — so group text is not reachable at
/// expansion time without threading group spans through `Match`/`find_all`/`find_at`. An unsupported
/// `\<d>` keeps the digit literal rather than erroring.
pub(crate) fn expand_replacement(replacement: &str, matched: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut st = CaseState::default();
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => st.emit(matched, &mut out),
            '\\' => match chars.next() {
                Some('0') => st.emit(matched, &mut out),
                Some('n') => st.emit("\n", &mut out),
                Some('t') => st.emit("\t", &mut out),
                Some('r') => st.emit("\r", &mut out),
                Some('&') => st.emit("&", &mut out),
                Some('\\') => st.emit("\\", &mut out),
                Some('u') => st.pending = Some(Case::Upper),
                Some('l') => st.pending = Some(Case::Lower),
                Some('U') => st.region = Some(Case::Upper),
                Some('L') => st.region = Some(Case::Lower),
                Some('e' | 'E') => st.region = None,
                Some(other) => {
                    let mut buf = [0u8; 4];
                    st.emit(other.encode_utf8(&mut buf), &mut out);
                }
                None => st.emit("\\", &mut out),
            },
            _ => {
                let mut buf = [0u8; 4];
                st.emit(c.encode_utf8(&mut buf), &mut out);
            }
        }
    }
    out
}
