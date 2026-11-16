//! Resolved Ex edits (RFC-0020, D-061): the form an Ex command takes on the [`Command`] boundary, so it is
//! recorded in the trace and replays without an Ex parser, cursor context, history or IO.
//!
//! Every field is a value. Ranges are the 1-based inclusive `(first, last)` lines that ran, destinations the
//! resolved insert index, patterns and replacements the literal text used (after the empty pattern became
//! the last search and `&` became the remembered substitute), and the input of `:r` / `:{range}!` is the
//! bytes the frontend read. Each verb runs the same [`EditorState`] method the live command runs, so live
//! and replay share one implementation.
//!
//! [`Command`]: crate::command::Command

use crate::cells::VarStops;

use super::{EditorOption, GlobalCmd, SortOptions, SubFlags, SubOutcome};
use crate::pattern::RegexError;
use crate::pos::BytePos;
use crate::register::Register;
use crate::transaction::TransactionOrigin;
// Named only by doc links, now that `apply_ex` and the codec live in child modules.
#[cfg(doc)]
use super::EditorState;
#[cfg(doc)]
use crate::register::RegisterStore;

mod apply;
mod codec;
#[cfg(test)]
mod samples;
#[cfg(test)]
pub(crate) use samples::samples;

/// The most indent levels one `:>`/`:<` applies (`:>>>` is 3). The Ex parser counts repeated verb chars, so
/// a typed command never comes near it; the bound keeps a hostile trace line from shifting for minutes.
pub const MAX_SHIFT_LEVELS: u32 = 1000;

/// The most edits one [`EditorState::apply_edits`] batch holds, live and in a trace's `ex_edits` record
/// (RFC-0020 R6). A language server's response is one frame of at most 64 MiB, and an LSP `TextEdit` takes
/// more than 64 bytes of it, so no batch a server sends comes near.
pub const MAX_APPLY_EDITS: usize = 1 << 20;

/// The most replacement-text bytes one [`EditorState::apply_edits`] batch holds (RFC-0020 R6): nvim's
/// `INT_MAX` text limit, as for a register ([`crate::register::MAX_REGISTER_BYTES`]).
pub const MAX_APPLY_EDIT_BYTES: usize = i32::MAX as usize;

/// What opened an undo group whose children follow in the trace ([`ExCmd::Begin`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupKind {
    /// `:normal {keys}`: the commands its keys produced, and the cursor placement per line of its range.
    Normal,
    /// `:g/pat/normal {keys}`: the same, per marked line.
    Global,
    /// A macro replay (`@{reg}`, `@:`, `Q`): its keys were not typed, so Vim records one undo step.
    Macro,
}

/// One resolved Ex edit, or a piece of frontend state an edit depends on (RFC-0020 R1–R4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExCmd {
    /// `:s`, `&`, `g&`, `:&`, `:&&`: the lines, literal pattern/replacement and effective flags used.
    /// `answers` is `Some` for `:s///c`: the user's answer per match, in order (missing ones are "no").
    Substitute {
        first: usize,
        last: usize,
        pattern: String,
        replacement: String,
        flags: SubFlags,
        answers: Option<Vec<bool>>,
    },
    /// `:g/pat/d` / `:g/pat/s///`: the 1-based lines the pattern marked, and the command run on them.
    Global { marks: Vec<usize>, cmd: GlobalCmd },
    /// `:d [x]`.
    Delete {
        first: usize,
        last: usize,
        reg: Option<char>,
    },
    /// `:y [x]`.
    Yank {
        first: usize,
        last: usize,
        reg: Option<char>,
    },
    /// `:j[!]`.
    Join {
        first: usize,
        last: usize,
        no_space: bool,
    },
    /// `:>` / `:<`, `levels` times.
    Shift {
        first: usize,
        last: usize,
        left: bool,
        levels: u32,
    },
    /// `:m {dest}`: `dest` is the 0-based insert index ("after line `dest`").
    Move {
        first: usize,
        last: usize,
        dest: usize,
    },
    /// `:t {dest}` / `:co`.
    Copy {
        first: usize,
        last: usize,
        dest: usize,
    },
    /// `:put [x]` / `:put! [x]`: after line `dest` (the insert index).
    Put { dest: usize, reg: Option<char> },
    /// `:sort`.
    Sort {
        first: usize,
        last: usize,
        opts: SortOptions,
    },
    /// `:retab[!] [N[,N…]]`: the new tab stops, one (`'tabstop'`) or a list (`'vartabstop'`, #1163).
    Retab {
        first: usize,
        last: usize,
        bang: bool,
        tabstop: Option<VarStops>,
    },
    /// `:r {file}` / `:r !{cmd}`: the bytes read, inserted after line `dest`. `cursor_on_last` is the
    /// command read's landing line (the last inserted one).
    ReadText {
        dest: usize,
        cursor_on_last: bool,
        bytes: Vec<u8>,
    },
    /// `:{range}!{cmd}`: the filter's output that replaced the lines.
    FilterReplace {
        first: usize,
        last: usize,
        bytes: Vec<u8>,
    },
    /// `:[range]mark {x}` / `:k{x}`: mark `name` (any mark `:mark` sets, #1241) on 1-based line `line`, at
    /// its first non-blank as nvim's `ex_mark` puts it when `nonblank`, else at its start: a trace written
    /// before #1241 records only letters, at the line start, and replays so (`ex_mark {line} {name}`).
    Mark {
        line: usize,
        name: char,
        nonblank: bool,
    },
    /// `:set` of an option the core reads while editing; also the trace's starting option values.
    Set(EditorOption),
    /// The cursor placed at a byte offset by the frontend (`:normal`'s per-line column 0, a `:g` landing
    /// line, a `:s///c` match left under the cursor). Leaves the wanted column alone, as the live placement.
    Place(BytePos),
    /// The wanted column (Vim's `curswant`) set by the frontend: `MAXCOL` after `$`. A switch to
    /// another window on the same buffer brings that window's cursor ([`ExCmd::Place`]) and its wanted
    /// column, which the next `j`/`k`/`G` reads (#1272).
    Want(usize),
    /// Vim's `u_sync` before a typed command: the next change starts a new undo step.
    UndoSync,
    /// An edit list that came from outside the core (RFC-0020 R6): a language server's formatting, rename,
    /// code action or completion edits, resolved to the disjoint `(start, end, text)` byte edits the session
    /// passed to [`EditorState::apply_edits`], with their origin. Replay calls the same `apply_edits`.
    Edits {
        origin: TransactionOrigin,
        edits: Vec<(BytePos, BytePos, String)>,
    },
    /// The `:keepjumps` switch (R6): while on, no jumplist or change-list entry is recorded.
    KeepJumps(bool),
    /// `:set (no)modifiable` on the focused buffer (#619): a replay refuses the same edits live did.
    Modifiable(bool),
    /// `:set (no)modified` on the focused buffer (#619): marks it unsaved / saved.
    Modified(bool),
    /// A register value the session wrote that no recorded command produced (R6): the read-only
    /// `"/ ": ". "% "#` synced from frontend state, a clipboard read, a macro's raw keys, and the registers
    /// filled when the trace began. Sets exactly that slot ([`RegisterStore::set_slot`]); `None` is the
    /// unnamed register.
    Register { name: Option<char>, value: Register },
    /// The start of a group of child commands that undo as one step.
    Begin(GroupKind),
    /// The end of the innermost open [`ExCmd::Begin`].
    End,
}

/// What an [`ExCmd`] did, for the frontend's status line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExOutcome {
    /// Nothing to report (a `Set`, `Place`, group marker).
    Done,
    /// A line count: deleted, yanked, joined, shifted, put, read, filtered, acted on by `:g`, or removed by
    /// `:sort u`.
    Lines(usize),
    /// `:m`/`:t`: the lines moved or copied, `None` when a move lands inside its own range.
    Relocated(Option<usize>),
    /// `:s`: replacements and lines.
    Substituted(SubOutcome),
    /// `:retab`: whether a line changed.
    Retabbed(bool),
    /// An edit list: whether it applied (`apply_edits` refuses a bad batch whole).
    Applied(bool),
    /// A pattern that does not compile; nothing changed.
    Failed(RegexError),
    /// The buffer is `'nomodifiable'` (`E21`, #619); nothing changed.
    NotModifiable,
}

impl ExCmd {
    /// Whether the command changes the text (every edit `'nomodifiable'` refuses).
    #[must_use]
    pub fn edits(&self) -> bool {
        matches!(
            self,
            ExCmd::Substitute { .. }
                | ExCmd::Global { .. }
                | ExCmd::Delete { .. }
                | ExCmd::Join { .. }
                | ExCmd::Shift { .. }
                | ExCmd::Move { .. }
                | ExCmd::Copy { .. }
                | ExCmd::Put { .. }
                | ExCmd::Sort { .. }
                | ExCmd::Retab { .. }
                | ExCmd::ReadText { .. }
                | ExCmd::FilterReplace { .. }
                | ExCmd::Edits { .. }
        )
    }
}
