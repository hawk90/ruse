//! The plan's vocabulary: the [`Plan`] a pure `plan` returns and the [`Action`], register write
//! ([`RegWrite`]), mark write ([`MarkWrite`]) and blockwise-insert session ([`BlockInsert`], [`BlockGeom`])
//! it carries for `commit` to apply. Moved out of `editor/mod.rs` (#1495); `pub(super)` keeps each item's
//! old reach, the `editor` module and its children.

use crate::{BlockInsertKind, EditList, Effect, GroupHint, Mode, Register};

pub(super) enum Action {
    Txn {
        edits: EditList,
        hint: GroupHint,
    },
    /// A Replace-mode edit that also updates the `<BS>`-restore stack: apply `edits`, `pop` that many
    /// entries (a backspace), then `push` one entry per char now before the cursor (`Some(orig)` overwrote
    /// it / `None` added it). Distinct from `Txn` only because the stack mutation must ride with the edit.
    ReplaceTxn {
        edits: EditList,
        hint: GroupHint,
        push: Vec<Option<Vec<u8>>>,
        pop: usize,
    },
    Undo,
    Redo,
    /// `U`: restore the latest changed line ([`Document::line_undo`]); the cursor comes from the swap.
    LineUndo,
    /// `g-`/`g+`: step along chronological creation order (`older` = `g-`), across branches.
    UndoChrono {
        older: bool,
    },
    /// `g;`/`g,`: step the change list (`older` = `g;`) and move the cursor to that change. The step
    /// mutates `change_idx`, so it happens in [`commit`] (the planner is pure); a no-op at either end.
    JumpChange {
        older: bool,
    },
    /// `CTRL-O`/`CTRL-I`: step the jumplist (`older` = `CTRL-O`) and move the cursor there. Mutates the
    /// jumplist (saves the current pos on the first `CTRL-O`), so it happens in [`commit`]; no-op at an end.
    JumpList {
        older: bool,
    },
    /// `m{a-z}`: install named mark `ch` at the current cursor. Mutates the mark table, so it applies in
    /// [`commit`]; the cursor does not move.
    SetNamedMark {
        ch: char,
    },
    Nop,
    /// Install the one-shot pending register (`"x`). Distinct from `Nop` so [`commit`] knows NOT to clear
    /// the pending register it just set — every other action clears it once its command has consumed it.
    SetPending(Option<char>),
    /// Store an evaluated expression result into the `"=` register and arm `=` as the one-shot pending
    /// register (`"=<expr><CR>`). The carried bytes are the already-formatted result (EMPTY on an
    /// evaluation error); [`commit`] writes it into the slot and sets the pending register, then returns
    /// early exactly like [`Action::SetPending`] so the following `p`/`P` reads it.
    SetExprPending(Vec<u8>),
    /// Arm a blockwise insert-replicate session (`CTRL-V` then `I`/`A`/`c`): apply `edits` (the block
    /// delete for `c`, and/or top-row padding for `A`), then install `session` so the following `<Esc>`
    /// replicates the text typed on the top row down every other block row (see [`BlockInsert`]).
    BlockInsertArm {
        edits: EditList,
        hint: GroupHint,
        session: BlockInsert,
    },
}

/// A live blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`). While armed, typing lands on the
/// block's top row as normal Insert; on `<Esc>` ([`Command::EnterNormal`]) the text typed since
/// `insert_start` is inserted at `target_col` on each of the `rows_below` rows beneath the top line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct BlockInsert {
    /// Byte offset where the insert began on the top row — the start of the replicated text at `<Esc>`.
    pub(super) insert_start: usize,
    /// Byte offset of the block's top-LEFT corner (left edge on the top row) — where the cursor rests when
    /// the session ends. Equals `insert_start` for `I`/`c`; for `A` it is the left edge, not the append col.
    pub(super) top_left: usize,
    /// Byte offset of the top block row's line start — the stable anchor to walk down from at `<Esc>`.
    pub(super) top_line_start: usize,
    /// The top row's length in bytes when Insert began (after a change's delete; nvim's `pre_textlen`):
    /// what a block change measures the text typed on it against when a `<CR>` ended that row (#1142).
    pub(super) top_len: usize,
    /// Virtual (screen) column at which each lower row receives the typed text (left edge for `I`/`c`,
    /// right edge+1 for `A`).
    pub(super) target_col: usize,
    /// Number of block rows below the top row (the rows to replicate onto).
    pub(super) rows_below: usize,
    /// `A`/append semantics: pad rows shorter than `target_col` with spaces. `false` (`I`/`c`) skips a row
    /// that does not reach `target_col`.
    pub(super) append: bool,
    /// `$`-ragged append (`` <C-v>$A ``): insert at EACH row's own line-end rather than a fixed `target_col`,
    /// so trailing text aligns to variable line lengths (Vim). Only meaningful with `append`.
    pub(super) to_eol: bool,
    /// `c`/`s` (change) rather than `I`/`A` (insert/append). Only the cursor RESTING position differs: Vim
    /// leaves a block CHANGE with the caret one char left of the top row's typed text (normal Insert-exit),
    /// whereas `I`/`A` snap back to the block's top-left corner. The replicate geometry is identical.
    pub(super) change: bool,
    /// The block's width in cells (`hi - lo + 1`, >= 1) — the number of columns `A` appends
    /// past and `c` deletes per row. Retained (with the row count) into [`BlockGeom`] so dot-repeat can
    /// rebuild an equivalent block at the caret. `I` ignores it (insert at the left edge), but it is stored
    /// uniformly so the geometry round-trips for every kind.
    pub(super) width: usize,
}

/// The geometry of a committed blockwise insert-replicate (`CTRL-V I/A/c`), the minimum a dot-repeat needs
/// to reconstruct an equivalent block at a NEW caret: nvim positions the repeat's top-left at the cursor and
/// reuses the original `width` x `height` (verified vs nvim v0.12.4). `kind`/`to_eol` reproduce the I/A/c
/// variant and the `$`-ragged (`$A`) append. The caret column and row come from the live cursor at `.` time,
/// so only the SIZE and MODE are stored here — not the original position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct BlockGeom {
    /// Total block rows (>= 1) — how many lines the repeat replicates onto, clamped at end-of-buffer.
    pub(super) height: usize,
    /// The block's width in cells (>= 1) — what `A` appends past / `c` deletes per row (see
    /// [`BlockInsert::width`]).
    pub(super) width: usize,
    /// `I` (insert at left edge) / `A` (append at right edge) / `c` (delete then insert at left edge).
    pub(super) kind: BlockInsertKind,
    /// `$`-ragged append (`$A`): append at each row's own line-end rather than a fixed column. Only with `A`.
    pub(super) to_eol: bool,
}

/// How a committed command should route its captured text into the register store. A `Yank` additionally
/// seeds the yank register `"0` (when unregistered); an `Edit` (delete/change/`x`) never touches `"0`, so
/// `"0` survives intervening deletes (Vim `:help quote0`).
pub(super) enum RegWrite {
    Edit(Register),
    /// An `Edit` over a motion that always uses `"1` (`d%`, `d}`, `d/pat` — nvim's `use_reg_one`), so even a
    /// span within one line shifts the numbered ring (#749). [`apply_command`] promotes `Edit` to it.
    EditRingOne(Register),
    /// The text a Visual put (`v_p`) replaced: deleted into the unnamed register and the delete rings, never
    /// into the register being put (`viw"ap` leaves `"a` as it was, #707).
    Replaced(Register),
    /// A yank: the captured [`Register`] plus the `(start, end)` byte span (half-open) it was taken from,
    /// used to set the `[`/`]` change marks — a yank leaves the buffer unchanged, so the span cannot be
    /// recovered from an [`EditList`] the way an edit's affected range can.
    Yank(Register, (usize, usize)),
    /// An Emacs KILL (`kill-line`/`kill-word`/`kill-region`). Like `Edit` it captures on a delete, but it
    /// ACCUMULATES: when the previous command was also a kill ([`View::last_was_kill`]) the captured text is
    /// appended onto the current unnamed entry rather than overwriting it (Emacs kill-ring behaviour). A
    /// kill always leaves `last_was_kill` set so the following kill accumulates onto it.
    KillAppend(Register),
    /// A BACKWARD Emacs kill (`backward-kill-word`). Same accumulation rule as [`RegWrite::KillAppend`], but
    /// when it follows another kill the captured text is PREPENDED onto the current entry (Emacs
    /// `kill-append` with `before=t`), since a backward kill takes text that precedes the prior kill.
    KillPrepend(Register),
    /// A plain write of new text into the unnamed slot (Emacs `kill-new`, for a pasted text): never
    /// accumulates onto a previous kill and is not itself a kill.
    Write(Register),
}

/// How a committed command updates the Emacs mark (D-027 depth-1). `None` on a plan leaves the mark
/// untouched — the common case, since the mark survives ordinary motions and edits.
pub(super) enum MarkWrite {
    /// Install the mark at this byte offset (`C-SPC`; the old point on `C-x C-x`; the region start on
    /// `C-w`). Mark DEACTIVATION (`C-g`, and the active/inactive distinction) belongs to the deferred
    /// mark-ring slice (D-027) — nothing drops the mark yet, so there is no `Clear` today.
    Set(usize),
}

/// The pure result of [`plan`]: what a command would do, before any mutation.
pub struct Plan {
    pub(super) action: Action,
    pub(super) cursor: usize,
    pub(super) mode: Mode,
    pub(super) is_edit: bool,
    pub(super) effects: Vec<Effect>,
    /// Text to store into a register on commit — an `Edit` capture (delete/change) or a `Yank` (which also
    /// seeds `"0`). `None` when the command writes no register.
    pub(super) set_register: Option<RegWrite>,
    /// A new selection anchor to install on commit — set only when a text object issued in a selection mode
    /// (`viw`/`vi(`) must move BOTH ends, unlike a bare motion that only moves the cursor. `None` leaves the
    /// anchor to the mode-transition logic in [`commit`].
    pub(super) set_anchor: Option<usize>,
    /// How this command updates the Emacs mark (D-027). `None` leaves it untouched — the common case, so
    /// only the mark commands (`SetMark`/`KillRegion`/`ExchangePointMark`) set it.
    pub(super) set_mark: Option<MarkWrite>,
    /// Where the command leaves the change marks `'[` `']` `'.` when nvim's operator puts them somewhere
    /// other than the default (the edit span's ends and the cursor, see `set_change_marks`). `None` keeps
    /// the default. Set even on a no-op when nvim's operator still marks it (a block `<` that moves no
    /// text: nvim's `op_shift` sets the marks and calls `changed_lines` whatever it changed).
    pub(super) op_marks: Option<OpMarks>,
    /// How the plan's edits map onto Vim undo entries — one per edit, or one over all the lines a blockwise
    /// operator touches (nvim's `op_shift`/`op_delete`/`op_tilde`/`op_replace` `u_save` the block's lines at
    /// once): where `u`/`CTRL-R` put the cursor and the `'.` mark (#1612).
    pub(super) undo_entry: crate::undo_cursor::UndoEntry,
}

/// The change marks an operator leaves, as nvim's `b_op_start` / `b_op_end` / `b_last_change` (#1302). Each
/// is a 0-based line of the text AFTER the command and a column on it, so a planner can name them before
/// the edit is applied: nvim's operators set these marks from line numbers and raw columns, not from the
/// edited bytes. Resolved onto the text in `commit`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct OpMarks {
    /// `'[`.
    pub(super) start: MarkAt,
    /// `']`.
    pub(super) end: MarkAt,
    /// `'.`, the newest change-list entry (nvim's `changed_common` puts it where `changed_lines` /
    /// `changed_bytes` was last called).
    pub(super) dot: MarkAt,
}

/// One change mark of [`OpMarks`]: a 0-based line of the text after the command and a column on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct MarkAt {
    pub(super) line: usize,
    pub(super) col: MarkCol,
}

/// The column of a [`MarkAt`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MarkCol {
    /// A raw byte column. Past the line's length it clamps onto the line's end (its newline), and inside a
    /// multibyte char onto that char's first byte, for a jump; `'[` `']` keep it as their raw column
    /// (#1615), as nvim's `getpos()` reports it.
    Byte(usize),
    /// The line's length: the newline, as nvim's `do_join` puts `']` at `sumsize`.
    LineEnd,
    /// The line's last byte, or column 0 on an empty line: nvim's `ml_get_len(lnum) - 1` (`op_shift`'s
    /// `']`). On a line ending in a multibyte char nvim's mark is that char's last byte; ruse's marks stay
    /// on char boundaries, so commit's offset snap moves it to the char's first byte.
    LastByte,
}

impl MarkAt {
    pub(super) fn new(line: usize, col: MarkCol) -> Self {
        Self { line, col }
    }
}
