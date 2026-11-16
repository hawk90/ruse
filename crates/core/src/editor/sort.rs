//! The `:sort` options ([`SortOptions`], [`SortKind`]) the frontend parses and
//! [`EditorState::sort_lines`](super::EditorState::sort_lines) takes. Moved out of `editor/mod.rs` (#1495).

/// The options a `:sort` carries (parsed by the frontend from `[!][i][n][r][u] [/pattern/]`). Grouped into
/// one struct so [`EditorState::sort_lines`](super::EditorState::sort_lines) and the `Workspace` wrapper take a single argument.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct SortOptions {
    /// `!` — sort descending.
    pub reverse: bool,
    /// How keys compare: as text (the default), in locale order (`l`), or by the first number in them
    /// (`n` decimal, `x` hex, `o` octal, `b` binary, `f` float).
    pub key: SortKind,
    /// `u` — drop runs of equal sort KEYS after sorting.
    pub unique: bool,
    /// `i` — compare case-insensitively.
    pub ignore_case: bool,
    /// `/pattern/` — sort on the text AFTER the first match (or the matched text under `use_match`), not the
    /// whole line. `None` sorts on the whole line.
    pub pattern: Option<String>,
    /// `r` — with a pattern, sort on the MATCHED text itself rather than what follows it.
    pub use_match: bool,
}

/// How `:sort` compares its keys (`:help :sort`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum SortKind {
    /// Byte order (case-folded under `i`).
    #[default]
    Text,
    /// `l` — the locale's collation order. ruse does not read the system locale: letters compare
    /// ignoring case first, and a lowercase letter goes before its uppercase form, which is the order the
    /// common UTF-8 locales (`en_US.UTF-8`) give ASCII text.
    Locale,
    /// `n` — the first decimal number.
    Decimal,
    /// `x` — the first hexadecimal number (an optional `0x`/`0X` prefix).
    Hex,
    /// `o` — the first octal number.
    Octal,
    /// `b` — the first binary number (an optional `0b`/`0B` prefix).
    Binary,
    /// `f` — the first floating-point number, read as C's `strtod` does.
    Float,
}
