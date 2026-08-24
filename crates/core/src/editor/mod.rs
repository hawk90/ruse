//! The editor state and the pure **plan / commit** command pipeline (RFC-0012).
//!
//! [`plan`] is a *pure decision*: `(&EditorState, &Command) -> Plan`, no mutation, no IO. [`commit`] applies
//! a `Plan` and returns the [`Effect`]s the frontend must perform. Because the core never does IO, replaying
//! the same commands on the same initial document is deterministic (see [`crate::trace`]). This is the split
//! that captures most of a Haskell rewrite's benefit in Rust — enforced by an empty dependency set.

use crate::command::{
    BlockInsertKind, Command, ForcedWise, OpKind, OpenKind, SearchOp, SelectKind, WordCase,
};
use crate::document::{Document, DocumentId};
use crate::edit::{Edit, EditList};
use crate::effect::Effect;
use crate::motion::{
    self, at_col, col_of, line_end, line_start, next_boundary, next_grapheme, prev_boundary,
    prev_grapheme, snap, Motion,
};
use crate::pattern::RegexError;
use crate::register::{Register, RegisterStore};
use crate::transaction::{GroupHint, Transaction, TransactionOrigin};

/// The editor mode. `Visual { kind }` and `Select { kind }` are selection modes carrying a [`SelectKind`]
/// shape: charwise (`v`), linewise (`V`), or blockwise (`CTRL-V`). A selection is the pair `(anchor,
/// cursor)`; the shape reinterprets that pair (charwise = one byte range, blockwise = a column-aligned
/// rectangle). A bare caret is the degenerate collapsed selection, so extending to multi-selection later
/// needs no type rewrite (D-027 trajectory).
///
/// `Select` shares the SAME selection state as `Visual` (they toggle with `CTRL-G`) and differs only in
/// its unmatched-key policy: in Select a printable key deletes the selection and enters Insert
/// (`open/replace-selection`), where Visual ignores it. This is the census's own framing — two modes over
/// identical state distinguished by the one dimension a transition table could not record
/// (contracts/vim-style.yaml, `namespaces.select`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    /// Replace mode (`R`): a printable key OVERWRITES the char under the cursor instead of inserting; at
    /// end-of-line it appends. `<BS>` restores the overwritten char (or deletes an appended one).
    Replace,
    /// Virtual Replace mode (`gR`): like [`Mode::Replace`] but TAB-AWARE — a `<Tab>` under the cursor is
    /// eaten one virtual (display) column at a time (the char is inserted before the tab, shrinking it,
    /// until its last column is consumed and the tab is replaced), so the on-screen layout is preserved.
    /// Shares the `<BS>`-restore stack with Replace.
    VirtualReplace,
    Visual {
        kind: SelectKind,
    },
    Select {
        kind: SelectKind,
    },
    /// Terminal mode (F-011): keys are forwarded to the PTY child, not the editing grammar. The frontend
    /// owns the PTY + scrollback; the core only carries the mode so it is per-view (VS-OBL-1) and drives the
    /// status line / input routing. No editing command touches a terminal buffer's (placeholder) document.
    Terminal,
    /// Terminal-Normal mode (F-011, `t_CTRL-\ CTRL-N`): read-only navigation of the terminal scrollback with
    /// the normal motion grammar; `i`/`a` return to [`Mode::Terminal`]. Edits are suppressed by the frontend.
    TerminalNormal,
}

/// The indentation unit a shift operator (`>>`/`<<`) applies, derived from the two editor config keys
/// `editor.indent_style` (space|tab) and `editor.tab_width`. Modelled here rather than as ad-hoc constants
/// so the shift commands read the same knobs a future runtime config loader will set — no NEW schema key is
/// introduced (spec/config-schema.yaml already owns both). Defaults match the schema: spaces, width 4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndentStyle {
    /// `editor.indent_style = space`: one indent level is `editor.tab_width` spaces.
    Space,
    /// `editor.indent_style = tab`: one indent level is a single `\t`.
    Tab,
}

/// One `:set` option this MVP honors, mapped onto the existing indent/search-case config. `bool` options
/// carry their on/off value (`:set ic` = `IgnoreCase(true)`, `:set noic` = `IgnoreCase(false)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditorOption {
    /// `'ignorecase'` (`ic`).
    IgnoreCase(bool),
    /// `'smartcase'` (`scs`).
    SmartCase(bool),
    /// `'shiftwidth'` (`sw`) — one indent level's width (also drives `editor.tab_width` here).
    ShiftWidth(usize),
    /// `'expandtab'` (`et`) — indent with spaces (`true`) vs a tab (`false`).
    ExpandTab(bool),
    /// `'textwidth'` (`tw`) — the width `gq`/`gw` wrap to; `0` means use the 79-column fallback.
    TextWidth(usize),
}

/// The indent config a shift/indent operator (`>>`/`<<`) reads: `editor.tab_width` +
/// `editor.indent_style`. Grouped so the "config" concern is one field on [`View`], not four loose ones.
#[derive(Clone, Copy)]
struct IndentConfig {
    /// `editor.tab_width` — one indent level's width in columns/spaces. Schema default 4.
    tab_width: usize,
    /// `editor.indent_style` — whether an indent level is spaces or a tab. Schema default `space`.
    style: IndentStyle,
}

/// The search-case config: `'ignorecase'` and `'smartcase'` (F-009). Default off — the factory Vim
/// default the differential oracle runs against.
#[derive(Clone, Copy)]
struct SearchCase {
    ignore: bool,
    smart: bool,
}

impl Mode {
    /// Whether this mode carries a live selection (Visual or Select) — the anchor is `Some` exactly then.
    /// The [`SelectKind`] is the selection's shape when it has one.
    #[must_use]
    fn selection(self) -> Option<SelectKind> {
        match self {
            Mode::Visual { kind } | Mode::Select { kind } => Some(kind),
            Mode::Normal
            | Mode::Insert
            | Mode::Replace
            | Mode::VirtualReplace
            | Mode::Terminal
            | Mode::TerminalNormal => None,
        }
    }
}

/// How a caret rests relative to text (D-050 / RFC-0015). A View-local property SELECTED BY THE INPUT
/// PROFILE, orthogonal to [`Mode`]: it decides whether the line-/buffer-end position is the last character
/// or the empty slot after it, which is where Vim and Emacs disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CaretGravity {
    /// Vim Normal mode: the caret rests ON a character; on a non-empty line it can rest at most on the LAST
    /// character, never the slot after it. The Normal-mode edit clamp enforces this. The default.
    #[default]
    OnChar,
    /// Emacs point (also Vim Insert): the caret rests BETWEEN characters; the line/buffer end is the slot
    /// AFTER the last character. The Emacs profile constructs its View with this so edits are not Vim-clamped.
    BetweenChar,
}

/// Editor state over one document: the buffer, a byte cursor (always on a char boundary), and the mode.
/// View-local state lives here for the headless spine; a real multi-view split comes later (INV-DOC-VIEW).
/// A **View**: the per-view state over a shared [`Document`] (F-007 Buffer/View split). Cursor,
/// mode, selection and the transient session state are view-local — INV-DOC-VIEW — so two Views of
/// one buffer have independent cursors. Registers and the indent config sit here for now (a single
/// view); a future step lifts them to workspace scope (see the F-007 backlog). The Document is NOT
/// here: the [`View`] only NAMES its buffer by [`DocumentId`] (never a borrow), so N Views sharing
/// one buffer is N Views naming one id — the [`crate::workspace::Workspace`] arena owns the Document.
#[derive(Clone)]
pub struct View {
    /// The buffer this View shows, named by handle (never a borrow) so many Views can share one
    /// Document without interior mutability or reference cycles (INV-DOC-VIEW / INV-HANDLE).
    doc: DocumentId,
    /// First visible buffer row — this View's scroll position. View-local so two Views of one buffer
    /// scroll independently (F-007 acceptance #1). Maintained by the frontend viewport pass.
    top: usize,
    cursor: usize,
    mode: Mode,
    /// Whether the previous command edited text — drives undo grouping: an edit right after a non-edit
    /// (a motion or a mode change) starts a new undo group (persistence §6); consecutive edits coalesce.
    last_was_edit: bool,
    /// Whether the previous command was an Emacs KILL (`kill-line`/`kill-word`/`kill-region`) — the analogue
    /// of Emacs's `last-command == kill-region` test. When set, the next kill APPENDS onto the current
    /// unnamed-register entry instead of overwriting it (kill-accumulation); any non-kill command clears it,
    /// breaking the run. Set from [`RegWrite::KillAppend`] in [`commit`]; Vim edits never touch it.
    last_was_kill: bool,
    /// The register store: the unnamed slot plus named `a`–`z` (`A`–`Z` append). Text yanked (`y`) or
    /// deleted (`d`/`c`/`x`) lands here for a later paste (`p`/`P`); an edit into `"x` mirrors the unnamed
    /// slot too. The numbered delete-ring / `"0` yank register stay deferred (D-026; see `register.rs`).
    registers: RegisterStore,
    /// The one-shot register selected by `"x` — the slot the NEXT yank/delete/change/paste targets, then
    /// cleared. `None` = the unnamed register (the default). Set by [`Command::SetRegister`] and consumed by
    /// the following register command in [`commit`]; any other committed command also clears it (a stray
    /// `"x` followed by a plain motion forgets the selection, as in Vim).
    pending_register: Option<char>,
    /// The Visual-mode selection anchor (the fixed end; the cursor is the moving end). `Some` exactly while
    /// in `Mode::Visual`. The full anchor-store-backed `Selection` set is deferred (D-027).
    anchor: Option<usize>,
    /// The Emacs mark (F-012 / D-027 depth-1): a single per-buffer position defining the region `point..mark`,
    /// the degenerate one-caret `Ring<Selection>` of the full mark-ring. Independent of `anchor` — it survives
    /// across motions and edits (Emacs is non-modal, so the region is not a mode) until a kill or a new mark
    /// moves it. `None` when no mark is set. The per-buffer/global mark RINGS remain deferred (D-027).
    mark: Option<usize>,
    /// The LAST selection left behind, as `(anchor, active_end, linewise)` — captured whenever a Visual/
    /// Select mode is exited, restored by `gv` ([`Command::ReselectVisual`]). This is the depth-1
    /// degenerate of D-027's `` `< ``/`` `> `` selection history: one remembered selection, stored in the
    /// same raw-offset representation as the live `anchor` (both migrate to the anchor store together).
    last_visual: Option<(usize, usize, SelectKind)>,
    /// Replace-mode (`R`) session history for `<BS>` restore: one entry per key typed, in order. `Some(orig)`
    /// = that key OVERWROTE a char (its original bytes, to restore on backspace); `None` = it APPENDED at
    /// end-of-line (backspace deletes it). Empty outside a Replace session; cleared on leaving Replace.
    replace_stack: Vec<Option<Vec<u8>>>,
    /// A live blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`), armed by [`Action::BlockInsertArm`]
    /// and consumed by the next `<Esc>` ([`Command::EnterNormal`]). `None` outside such a session; cleared
    /// on any exit from Insert. See [`BlockInsert`].
    block_insert: Option<BlockInsert>,
    /// Whether the current Insert session opened a line with AUTO-INDENT and nothing non-blank has been
    /// typed on it since (Vim). While true, leaving Insert (`<Esc>`) on an all-whitespace line removes the
    /// auto-inserted indent so `o<Esc>` never leaves trailing whitespace. Set/cleared in [`apply_command`]
    /// (which sees the `Command`); read by the `EnterNormal` planner arm. Cleared on any non-blank insert
    /// and on every Insert entry/exit, so it never survives to a later line.
    auto_indent_pending: bool,
    /// The indent config (`editor.tab_width` + `editor.indent_style`).
    indent: IndentConfig,
    /// `'textwidth'` (`tw`): the column `gq`/`gw` wrap to; `0` = use the 79-column fallback (Vim).
    text_width: usize,
    /// The STICKY desired column (Vim `curswant`): the char column `j`/`k` aim for, preserved across shorter
    /// interior lines rather than collapsing to the short line's end. Maintained in [`apply_command`] — kept
    /// on a vertical move, set to [`MAXCOL`] by `$`/`<End>` (ride each line's end), and recomputed from the
    /// landing column after any other command. Read by the `plan` Move Up/Down arm via [`motion::vmove`].
    curswant: usize,
    /// The search-case config (`'ignorecase'` + `'smartcase'`, F-009).
    search_case: SearchCase,
    /// Caret gravity (D-050): `OnChar` for Vim/Neovim (default), `BetweenChar` for the Emacs profile. Gates
    /// the Normal-mode on-character edit clamp in [`commit`] so Emacs point rests after the last char.
    caret: CaretGravity,
    /// The change list (Vim `:changes`): recent EDIT positions, oldest → newest, bounded to
    /// [`MAX_CHANGES`]. Its last entry is the `` `. `` mark. Pushed in [`commit`] after any committing edit
    /// (adjacent identical positions coalesce) and snapped so a later buffer-resizing edit keeps every entry
    /// in range. Navigated by `g;` / `g,` through [`change_idx`].
    changes: Vec<usize>,
    /// The current cursor into [`changes`] for `g;`/`g,`. Equals `changes.len()` (past the newest) right
    /// after an edit, so the first `g;` lands on the newest change; walks toward 0 (oldest) on `g;` and back
    /// toward the newest on `g,`. Reset to `changes.len()` whenever a new change is pushed.
    change_idx: usize,
    /// The per-buffer named marks `a`–`z` (Vim `m{a-z}` / `` `{a-z} ``), indexed by `c - 'a'`. `None` = unset.
    /// Set by [`Command::SetNamedMark`], read by [`Command::GotoNamedMark`], and snapped on every commit like
    /// the change list. Uppercase/global marks and numbered marks stay deferred.
    named_marks: [Option<usize>; 26],
    /// Where Insert mode was last left (Vim's `` `^ `` mark), for `gi` to resume Insert there. Set in
    /// [`commit`] on an Insert→Normal transition to the pre-clamp caret, and snapped like the marks. `None`
    /// until the first Insert session ends.
    last_insert: Option<usize>,
    /// Vim's `[` mark: the byte offset of the FIRST char of the last changed OR yanked text. Set in
    /// [`commit`] after any committing edit (delete/replace/put), on leaving an Insert session, or on a
    /// yank; snapped every commit like the named marks. `None` before the first change/yank. Read by
    /// `` `[ `` (charwise) and `'[` (linewise, first non-blank of its line).
    change_start: Option<usize>,
    /// Vim's `]` mark: the byte offset bounding the END of the last changed/yanked text. For a
    /// delete/replace/put/yank it is the LAST char (inclusive); for an Insert session it is the insert
    /// END-caret (one past the last inserted char, matching Neovim), which the jump clamps onto the line's
    /// last char. Set/snapped alongside [`change_start`]. Read by `` `] `` / `']`.
    change_end: Option<usize>,
    /// Transient: where the CURRENT Insert-like session began (the caret when Insert/Replace was entered).
    /// Combined with the caret when the session is left, it bounds the inserted run for `[`/`]`. `None`
    /// outside a session. Snapped like the marks so a buffer-resizing edit under it stays valid.
    insert_start: Option<usize>,
    /// The jumplist (Vim): the cursor positions BEFORE each jump (search / `G` / `%` / a mark / paragraph),
    /// oldest → newest, bounded to [`MAX_CHANGES`]. Navigated by `CTRL-O` (older) / `CTRL-I` (newer) through
    /// [`jump_idx`]; recorded in [`apply_command`] and snapped every commit like the marks.
    jumps: Vec<usize>,
    /// The current cursor into [`jumps`] for `CTRL-O`/`CTRL-I`. Equals `jumps.len()` (past the newest) until
    /// the first `CTRL-O`, which saves the current position so `CTRL-I` can return, then walks the list.
    jump_idx: usize,
}

/// Maximum entries kept in a View's change list (Vim's default `:changes` history is ~100).
const MAX_CHANGES: usize = 100;

/// The editor over a single [`Document`] and its [`View`] — the top-level headless handle the TUI and
/// tests drive. F-007's Workspace will own many `(Document, View)` pairs referenced by handle; today
/// there is exactly one of each, composed here so the public API (`apply_command`, `cursor()`, …) is
/// unchanged while the buffer and view state are now cleanly separable.
pub struct EditorState {
    pub doc: Document,
    view: View,
}

impl View {
    /// A fresh view over `doc`: cursor at the start, Normal mode, empty registers/sessions. Config
    /// fields hold the schema defaults (spec/config-schema.yaml: editor.tab_width=4,
    /// editor.indent_style=space); runtime config wiring is deferred (as with editor.scrolloff).
    pub(crate) fn fresh(doc: DocumentId) -> View {
        View {
            doc,
            top: 0,
            cursor: 0,
            mode: Mode::Normal,
            last_was_edit: false,
            last_was_kill: false,
            registers: RegisterStore::new(),
            pending_register: None,
            anchor: None,
            mark: None,
            last_visual: None,
            replace_stack: Vec::new(),
            block_insert: None,
            auto_indent_pending: false,
            indent: IndentConfig {
                tab_width: 4,
                style: IndentStyle::Space,
            },
            text_width: 0,
            curswant: 0,
            search_case: SearchCase {
                ignore: false,
                smart: false,
            },
            caret: CaretGravity::OnChar,
            changes: Vec::new(),
            change_idx: 0,
            named_marks: [None; 26],
            last_insert: None,
            change_start: None,
            change_end: None,
            insert_start: None,
            jumps: Vec::new(),
            jump_idx: 0,
        }
    }

    /// The last-insert position (Vim `` `^ ``), or `None` before any Insert session has ended.
    fn last_insert(&self) -> Option<usize> {
        self.last_insert
    }

    /// The byte offset of named mark `c` (`a`–`z`), or `None` if unset / not a lowercase letter.
    fn named_mark(&self, c: char) -> Option<usize> {
        let i = (c as usize).checked_sub('a' as usize)?;
        self.named_marks.get(i).copied().flatten()
    }

    /// Set named mark `c` (`a`–`z`) to `pos`. A non-lowercase `c` is ignored (the input layer only sends
    /// `a`–`z`, so this is a defensive guard).
    fn set_named_mark(&mut self, c: char, pos: usize) {
        if let Some(i) = (c as usize).checked_sub('a' as usize) {
            if let Some(slot) = self.named_marks.get_mut(i) {
                *slot = Some(pos);
            }
        }
    }

    /// The position of the last change (Vim `` `. ``), or `None` before the first edit.
    #[must_use]
    pub fn last_change(&self) -> Option<usize> {
        self.changes.last().copied()
    }

    /// Vim's `[` mark — the first char of the last changed/yanked text, or `None` before the first change.
    pub fn change_mark_start(&self) -> Option<usize> {
        self.change_start
    }

    /// Vim's `]` mark — the end of the last changed/yanked text (see [`View::change_end`]), or `None`.
    pub fn change_mark_end(&self) -> Option<usize> {
        self.change_end
    }

    /// The context mark (Vim `` ` ``/`'`): the position before the latest jump — the newest jumplist entry.
    /// `None` before any jump. Read by `` `` ``/`''`; the jump they perform then records a fresh entry, so
    /// repeating toggles between the two positions.
    pub fn context_mark(&self) -> Option<usize> {
        self.jumps.last().copied()
    }

    /// Record `pos` as the newest change (Vim change list). Adjacent identical positions coalesce; the list
    /// is bounded to [`MAX_CHANGES`] (oldest dropped). Resets the `g;`/`g,` cursor past the newest entry.
    fn push_change(&mut self, pos: usize) {
        if self.changes.last() != Some(&pos) {
            self.changes.push(pos);
            if self.changes.len() > MAX_CHANGES {
                self.changes.remove(0);
            }
        }
        self.change_idx = self.changes.len();
    }

    /// Step the change-list cursor (`g;` = `older`, `g,` = newer) and return the position to jump to, or
    /// `None` when there is nowhere to go (empty list, or already at the oldest/newest end).
    fn nav_change(&mut self, older: bool) -> Option<usize> {
        if self.changes.is_empty() {
            return None;
        }
        if older {
            if self.change_idx == 0 {
                return None; // already at the oldest change
            }
            self.change_idx -= 1;
        } else {
            if self.change_idx + 1 >= self.changes.len() {
                return None; // already at (or past) the newest change
            }
            self.change_idx += 1;
        }
        self.changes.get(self.change_idx).copied()
    }

    /// Record `from` as a jumplist entry — the position a jump command LEFT (Vim). Adjacent identical
    /// positions coalesce; bounded to [`MAX_CHANGES`]; resets the `CTRL-O`/`CTRL-I` cursor past the newest.
    fn push_jump(&mut self, from: usize) {
        if self.jumps.last() != Some(&from) {
            self.jumps.push(from);
            if self.jumps.len() > MAX_CHANGES {
                self.jumps.remove(0);
            }
        }
        self.jump_idx = self.jumps.len();
    }

    /// Step the jumplist (`CTRL-O` = `older`, `CTRL-I` = newer) from the current position `now`, returning
    /// the position to jump to or `None` at an end. The FIRST `CTRL-O` saves `now` onto the list so a later
    /// `CTRL-I` returns to it (Vim); subsequent steps just walk the cursor.
    fn nav_jump(&mut self, older: bool, now: usize) -> Option<usize> {
        if older {
            if self.jump_idx == self.jumps.len() {
                // First move back: remember where we are so `CTRL-I` can return, then step onto the list.
                if self.jumps.last() != Some(&now) {
                    self.jumps.push(now);
                    if self.jumps.len() > MAX_CHANGES {
                        self.jumps.remove(0);
                    }
                }
                self.jump_idx = self.jumps.len().saturating_sub(1);
            }
            if self.jump_idx == 0 {
                return None; // already at the oldest jump
            }
            self.jump_idx -= 1;
        } else {
            if self.jump_idx + 1 >= self.jumps.len() {
                return None; // already at (or past) the newest jump
            }
            self.jump_idx += 1;
        }
        self.jumps.get(self.jump_idx).copied()
    }

    /// The regex compile options for a search in this view (magic default; case per config).
    fn search_options(&self) -> crate::pattern::Options {
        crate::pattern::Options {
            magic: crate::pattern::Magic::Magic,
            ignore_case: self.search_case.ignore,
            smart_case: self.search_case.smart,
        }
    }

    /// The buffer this View shows.
    #[must_use]
    pub fn doc(&self) -> DocumentId {
        self.doc
    }

    /// The cursor's byte offset (on a char boundary) — view-local.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Place the cursor at byte offset `pos` (the frontend uses this to follow a `:s///c` match; the
    /// caller passes an on-boundary offset). View-local; does not touch the document.
    pub fn set_cursor(&mut self, pos: usize) {
        self.cursor = pos;
    }

    /// Select this View's caret gravity (D-050): the Emacs profile constructs `BetweenChar` views so its
    /// edits are not Vim-clamped; Vim/Neovim keep the `OnChar` default. The Workspace profile-init seam.
    pub fn set_caret_gravity(&mut self, gravity: CaretGravity) {
        self.caret = gravity;
    }

    /// This View's mode — view-local (two Views of one buffer can be in different modes).
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// This View's scroll position (first visible buffer row). Maintained by the frontend.
    #[must_use]
    pub fn top(&self) -> usize {
        self.top
    }

    /// Set this View's scroll position (frontend viewport pass).
    pub fn set_top(&mut self, top: usize) {
        self.top = top;
    }

    /// The highlighted byte range `[start, end)` of the current charwise/linewise Visual selection over
    /// `bytes`, or `None` in Normal/Insert and for a blockwise selection (a rectangle is not one range —
    /// use [`View::block_spans`]). See [`EditorState::selection_span`] for the full contract.
    #[must_use]
    pub fn selection_span(&self, bytes: &[u8]) -> Option<(usize, usize)> {
        match self.mode.selection()? {
            SelectKind::Blockwise => None,
            kind => Some(selection_range(
                bytes,
                self.anchor?,
                self.cursor,
                kind == SelectKind::Linewise,
            )),
        }
    }

    /// The per-row highlighted byte ranges of the current BLOCKWISE selection over `bytes`, or `None`
    /// when the selection is not blockwise. See [`EditorState::block_spans`].
    #[must_use]
    pub fn block_spans(&self, bytes: &[u8]) -> Option<Vec<(usize, usize)>> {
        match self.mode.selection()? {
            SelectKind::Blockwise => Some(block_rows(bytes, self.anchor?, self.cursor).0),
            _ => None,
        }
    }
}

enum Action {
    Txn {
        edits: EditList,
        hint: GroupHint,
    },
    /// A Replace-mode edit that also updates the `<BS>`-restore stack: apply `edits`, then `push` an entry
    /// (`Some(orig)` overwrote / `None` appended) and/or `pop` one on backspace. Distinct from `Txn` only
    /// because the stack mutation must ride with the edit.
    ReplaceTxn {
        edits: EditList,
        hint: GroupHint,
        push: Option<Option<Vec<u8>>>,
        pop: bool,
    },
    Undo,
    Redo,
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
struct BlockInsert {
    /// Byte offset where the insert began on the top row — the start of the replicated text at `<Esc>`.
    insert_start: usize,
    /// Byte offset of the block's top-LEFT corner (left edge on the top row) — where the cursor rests when
    /// the session ends. Equals `insert_start` for `I`/`c`; for `A` it is the left edge, not the append col.
    top_left: usize,
    /// Byte offset of the top block row's line start — the stable anchor to walk down from at `<Esc>`.
    top_line_start: usize,
    /// Char column at which each lower row receives the typed text (left edge for `I`/`c`, right edge+1
    /// for `A`).
    target_col: usize,
    /// Number of block rows below the top row (the rows to replicate onto).
    rows_below: usize,
    /// `A`/append semantics: pad rows shorter than `target_col` with spaces. `false` (`I`/`c`) skips a row
    /// that does not reach `target_col`.
    append: bool,
    /// `$`-ragged append (`` <C-v>$A ``): insert at EACH row's own line-end rather than a fixed `target_col`,
    /// so trailing text aligns to variable line lengths (Vim). Only meaningful with `append`.
    to_eol: bool,
}

/// How a committed command should route its captured text into the register store. A `Yank` additionally
/// seeds the yank register `"0` (when unregistered); an `Edit` (delete/change/`x`) never touches `"0`, so
/// `"0` survives intervening deletes (Vim `:help quote0`).
enum RegWrite {
    Edit(Register),
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
}

/// How a committed command updates the Emacs mark (D-027 depth-1). `None` on a plan leaves the mark
/// untouched — the common case, since the mark survives ordinary motions and edits.
enum MarkWrite {
    /// Install the mark at this byte offset (`C-SPC`; the old point on `C-x C-x`; the region start on
    /// `C-w`). Mark DEACTIVATION (`C-g`, and the active/inactive distinction) belongs to the deferred
    /// mark-ring slice (D-027) — nothing drops the mark yet, so there is no `Clear` today.
    Set(usize),
}

/// The pure result of [`plan`]: what a command would do, before any mutation.
pub struct Plan {
    action: Action,
    cursor: usize,
    mode: Mode,
    is_edit: bool,
    effects: Vec<Effect>,
    /// Text to store into a register on commit — an `Edit` capture (delete/change) or a `Yank` (which also
    /// seeds `"0`). `None` when the command writes no register.
    set_register: Option<RegWrite>,
    /// A new selection anchor to install on commit — set only when a text object issued in a selection mode
    /// (`viw`/`vi(`) must move BOTH ends, unlike a bare motion that only moves the cursor. `None` leaves the
    /// anchor to the mode-transition logic in [`commit`].
    set_anchor: Option<usize>,
    /// How this command updates the Emacs mark (D-027). `None` leaves it untouched — the common case, so
    /// only the mark commands (`SetMark`/`KillRegion`/`ExchangePointMark`) set it.
    set_mark: Option<MarkWrite>,
}

impl EditorState {
    /// A fresh editor over `initial` bytes, cursor at the start, Normal mode, marked saved (loaded content).
    pub fn new(initial: impl Into<Vec<u8>>) -> EditorState {
        let mut doc = Document::new(DocumentId(1), initial);
        doc.mark_saved();
        let view = View::fresh(doc.id());
        EditorState { doc, view }
    }

    /// Reconstitute an editing context from a Document and a View taken out of a Workspace arena — the
    /// inverse of [`EditorState::into_parts`]. The [`crate::workspace::Workspace`] swaps the focused
    /// `(Document, View)` into an `EditorState` to run the UNCHANGED `plan`/`commit` pipeline, then
    /// swaps them back; this keeps the single-window path byte-identical (F-007 step (b)).
    #[must_use]
    pub(crate) fn from_parts(doc: Document, view: View) -> EditorState {
        EditorState { doc, view }
    }

    /// Split an editing context back into its owned Document and View, to return them to the arena.
    #[must_use]
    pub(crate) fn into_parts(self) -> (Document, View) {
        (self.doc, self.view)
    }

    /// Set the indentation config the shift operators (`>>`/`<<`) use. Runtime config loading is deferred;
    /// until it lands this is the seam a loader (or a test) uses to install `editor.tab_width` /
    /// `editor.indent_style`. No new schema key — both are existing keys (spec/config-schema.yaml).
    pub fn set_indent(&mut self, tab_width: usize, indent_style: IndentStyle) {
        self.view.indent.tab_width = tab_width.max(1);
        self.view.indent.style = indent_style;
    }

    /// Set the search case config (`'ignorecase'` / `'smartcase'`, F-009 #1). The seam a config loader
    /// (or a test) uses until runtime config lands; the default is Vim's factory case-sensitive search.
    pub fn set_search_case(&mut self, ignore_case: bool, smart_case: bool) {
        self.view.search_case.ignore = ignore_case;
        self.view.search_case.smart = smart_case;
    }

    /// Set one editor option (`:set`) on the focused view, leaving the others untouched. Maps the
    /// runtime `:set` surface onto the existing indent/search-case config (no new schema keys).
    pub fn set_option(&mut self, opt: EditorOption) {
        match opt {
            EditorOption::IgnoreCase(v) => self.view.search_case.ignore = v,
            EditorOption::SmartCase(v) => self.view.search_case.smart = v,
            EditorOption::ShiftWidth(n) => self.view.indent.tab_width = n.max(1),
            EditorOption::ExpandTab(v) => {
                self.view.indent.style = if v {
                    IndentStyle::Space
                } else {
                    IndentStyle::Tab
                }
            }
            EditorOption::TextWidth(n) => self.view.text_width = n,
        }
    }

    /// Execute `:[range]s/pattern/replacement/flags` (F-009 #2). Every substitution across the range is
    /// applied as ONE undo group (a single [`Transaction`]). Matches are found with the Vim-regex engine
    /// (magic + `\zs`/`\ze`); the REPORTED span is what gets replaced, so `:s/foo\zsbar/X/` rewrites only
    /// `bar`. The replacement supports `&`/`\0` (the whole reported match) and `\n`/`\t`/`\\`/`\&`
    /// escapes; capture backreferences `\1`-`\9` are a documented follow-up (they need the lowering to
    /// expose a group-index map). The cursor lands on the start of the last changed line (Vim).
    ///
    /// # Errors
    /// [`RegexError`] if the pattern is unrepresentable or malformed, or the buffer is not UTF-8.
    pub fn substitute(
        &mut self,
        range: SubRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> Result<SubOutcome, RegexError> {
        let subs = self.substitute_preview(range, pattern, replacement, flags)?;
        Ok(self.apply_substitutions(&subs))
    }

    /// Compute — but do NOT apply — the substitutions `:[range]s/pat/rep/flags` would make (F-009 #2).
    /// Returns each pending [`Substitution`] (absolute byte span + replacement) in document order, so an
    /// interactive `c`-confirm loop can present them one by one and apply only the accepted subset with
    /// [`EditorState::apply_substitutions`]. Offsets are valid only until the buffer is edited — which is
    /// why confirm collects the accepted set and applies it in one pass at the end.
    ///
    /// # Errors
    /// [`RegexError`] if the pattern is unrepresentable/malformed or the buffer is not UTF-8.
    pub fn substitute_preview(
        &self,
        range: SubRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> Result<Vec<Substitution>, RegexError> {
        // Case: an explicit `i`/`I` flag overrides the config (and disables smartcase for this command).
        let base = self.view.search_options();
        let opts = crate::pattern::Options {
            magic: base.magic,
            ignore_case: flags.ignore_case.unwrap_or(base.ignore_case),
            smart_case: flags.ignore_case.is_none() && base.smart_case,
        };
        let re = crate::pattern::Regex::compile(pattern, opts)?;

        let bytes = self.doc.bytes();
        let hay = std::str::from_utf8(bytes)
            .map_err(|_| RegexError::Syntax("buffer is not valid UTF-8".into()))?;
        let lines = line_spans(hay); // (start, end-excluding-newline) per line
        let cursor_line = crate::pos::line_of(hay.as_bytes(), self.view.cursor);
        let (first, last) = match range {
            SubRange::CurrentLine => (cursor_line, cursor_line),
            SubRange::WholeFile => (0, lines.len().saturating_sub(1)),
            // 1-based inclusive from the user; clamp into range.
            SubRange::Lines(a, b) => (
                a.saturating_sub(1).min(lines.len().saturating_sub(1)),
                b.saturating_sub(1).min(lines.len().saturating_sub(1)),
            ),
        };

        let mut out: Vec<Substitution> = Vec::new();
        for (li, &(ls, le)) in lines.iter().enumerate().take(last + 1).skip(first) {
            let line = &hay[ls..le];
            let matches: Vec<crate::pattern::Match> = if flags.global {
                re.find_all(line)
            } else {
                re.find_at(line, 0).into_iter().collect()
            };
            for m in matches {
                let matched = &line[m.start..m.end];
                out.push(Substitution {
                    start: ls + m.start,
                    end: ls + m.end,
                    replacement: expand_replacement(replacement, matched),
                    line: li,
                });
            }
        }
        Ok(out)
    }

    /// Apply a set of pending [`Substitution`]s as ONE undo group (a single [`Transaction`]) and move the
    /// cursor to the start of the last changed line (Vim). The subs must be disjoint and in document
    /// order (as [`EditorState::substitute_preview`] returns them, or an accepted subset of it).
    pub fn apply_substitutions(&mut self, subs: &[Substitution]) -> SubOutcome {
        if subs.is_empty() {
            return SubOutcome {
                replacements: 0,
                lines: 0,
            };
        }
        let mut lines_seen = std::collections::BTreeSet::new();
        let edits: Vec<Edit> = subs
            .iter()
            .map(|s| {
                lines_seen.insert(s.line);
                Edit::replace(s.start, s.end - s.start, s.replacement.clone())
            })
            .collect();
        let replacements = edits.len();
        let last_line_idx = subs.iter().map(|s| s.line).max();
        // Disjoint + ordered by construction; `new` re-validates (a bad accepted set is a caller bug).
        let list = EditList::new(edits).expect("substitutions are disjoint and ordered");
        let txn = Transaction::new(self.doc.revision(), list, TransactionOrigin::UserInput)
            .with_hint(GroupHint::BreakBefore);
        self.doc
            .apply(txn)
            .expect("substitute transaction applies cleanly");
        self.view.last_was_edit = true;
        if let Some(li) = last_line_idx {
            let nb = self.doc.bytes();
            self.view.cursor = crate::pos::nth_line_start(nb, li).min(nb.len());
        }
        SubOutcome {
            replacements,
            lines: lines_seen.len(),
        }
    }

    /// Execute `:[range]g/pat/cmd` (F-009 #4) as a genuine TWO-PASS: pass 1 marks every line in range
    /// whose text the pattern matches (or does NOT match, when `negate` — `:g!` / `:v`); pass 2 runs
    /// `cmd` on the marked lines. Marking happens against the ORIGINAL buffer, so pass-2 edits never
    /// change which lines were selected — the property the acceptance names. The whole command is ONE
    /// undo group. Returns the number of lines acted on.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern (or a `:g/pat/s///` sub-pattern) is unrepresentable/malformed.
    /// PASS 1 of `:g` in isolation: the 0-based indices of the lines in `range` whose text the `pattern`
    /// matches (or does NOT match, when `negate` — `:g!` / `:v`), evaluated against the untouched buffer.
    /// Shared by [`EditorState::global`] (the core `d`/`s` payloads) and the frontend's `:g/pat/normal`
    /// runner, which needs the SAME mark set but replays each marked line through the input engine.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern is unrepresentable/malformed, or the buffer is not valid UTF-8.
    pub fn global_marks(
        &self,
        range: SubRange,
        pattern: &str,
        negate: bool,
    ) -> Result<Vec<usize>, RegexError> {
        let opts = self.view.search_options();
        let re = crate::pattern::Regex::compile(pattern, opts)?;
        let bytes = self.doc.bytes();
        let hay = std::str::from_utf8(bytes)
            .map_err(|_| RegexError::Syntax("buffer is not valid UTF-8".into()))?;
        let lines = line_spans(hay);
        // `line_spans` appends a phantom empty span for the position after a final newline; Vim does not
        // count that as a line, so exclude it (a `:g`/`:v` must never mark or act on it — visible under a
        // `normal` payload, which would edit it). An empty or non-`\n`-terminated buffer keeps its span.
        let count = if hay.ends_with('\n') {
            lines.len().saturating_sub(1)
        } else {
            lines.len()
        };
        let last_idx = count.saturating_sub(1);
        // `:g` with no range is the WHOLE FILE (unlike `:s`, whose default is the current line).
        let (first, last) = match range {
            SubRange::CurrentLine | SubRange::WholeFile => (0, last_idx),
            SubRange::Lines(a, b) => (
                a.saturating_sub(1).min(last_idx),
                b.saturating_sub(1).min(last_idx),
            ),
        };
        if count == 0 {
            return Ok(Vec::new());
        }
        Ok((first..=last)
            .filter(|&li| {
                let (ls, le) = lines[li];
                re.find_at(&hay[ls..le], 0).is_some() != negate
            })
            .collect())
    }

    pub fn global(
        &mut self,
        range: SubRange,
        pattern: &str,
        negate: bool,
        cmd: &GlobalCmd,
    ) -> Result<usize, RegexError> {
        // PASS 1: mark the matching (or non-matching) lines against the untouched buffer (shared with the
        // frontend's `:g/pat/normal` runner via [`EditorState::global_marks`]).
        let marked = self.global_marks(range, pattern, negate)?;
        if marked.is_empty() {
            return Ok(0);
        }
        // Re-derive the untouched buffer's line spans for pass 2 (marking above did not mutate the buffer).
        let bytes = self.doc.bytes();
        let hay = std::str::from_utf8(bytes)
            .map_err(|_| RegexError::Syntax("buffer is not valid UTF-8".into()))?;
        let lines = line_spans(hay);

        // PASS 2: run the command over the marked lines.
        match cmd {
            GlobalCmd::Delete => {
                // Delete each marked line including its trailing newline; disjoint + ascending, so one
                // transaction from the original offsets deletes them all atomically.
                let edits: Vec<Edit> = marked
                    .iter()
                    .map(|&li| {
                        let (ls, le) = lines[li];
                        let end = if le < hay.len() { le + 1 } else { le };
                        Edit::delete(ls, end - ls)
                    })
                    .collect();
                let list =
                    EditList::new(edits).expect("whole-line deletes are disjoint and ordered");
                let txn = Transaction::new(self.doc.revision(), list, TransactionOrigin::UserInput)
                    .with_hint(GroupHint::BreakBefore);
                self.doc.apply(txn).expect("global-delete applies cleanly");
                self.view.last_was_edit = true;
                let nb = self.doc.bytes();
                self.view.cursor = self.view.cursor.min(nb.len());
                Ok(marked.len())
            }
            GlobalCmd::Substitute {
                pattern: sp,
                replacement,
                flags,
            } => {
                let opts = self.view.search_options();
                let sopts = crate::pattern::Options {
                    magic: opts.magic,
                    ignore_case: flags.ignore_case.unwrap_or(opts.ignore_case),
                    smart_case: flags.ignore_case.is_none() && opts.smart_case,
                };
                let sre = crate::pattern::Regex::compile(sp, sopts)?;
                let mut subs: Vec<Substitution> = Vec::new();
                for &li in &marked {
                    let (ls, le) = lines[li];
                    let line = &hay[ls..le];
                    let matches: Vec<crate::pattern::Match> = if flags.global {
                        sre.find_all(line)
                    } else {
                        sre.find_at(line, 0).into_iter().collect()
                    };
                    for m in matches {
                        subs.push(Substitution {
                            start: ls + m.start,
                            end: ls + m.end,
                            replacement: expand_replacement(replacement, &line[m.start..m.end]),
                            line: li,
                        });
                    }
                }
                Ok(self.apply_substitutions(&subs).lines)
            }
        }
    }

    /// `:[range]d` — delete the lines in `range` (each with its trailing newline) as one undo group,
    /// like a linewise `dd` over the span. No range = the cursor's line. Returns the number of lines
    /// deleted (0 on an empty buffer). The cursor lands at the start of the line that took their place.
    pub fn delete_lines(&mut self, range: SubRange) -> usize {
        let bytes = self.doc.bytes();
        let Ok(hay) = std::str::from_utf8(bytes) else {
            return 0;
        };
        let lines = line_spans(hay);
        let cursor_line = crate::pos::line_of(hay.as_bytes(), self.view.cursor);
        let (first, last) = resolve_line_range(range, &lines, cursor_line);
        let (fs, _) = lines[first];
        let (_, le) = lines[last];
        // Terminated last line: delete `[fs, le+1)` (the lines plus their trailing newlines). Unterminated
        // last line (deleting through EOF): delete `[fs-1, le)` to absorb the newline BEFORE the span so no
        // dangling blank line remains — except when `first` is line 0 (whole buffer), then `[0, le)`.
        let (start, end) = if le < hay.len() {
            (fs, le + 1)
        } else if fs > 0 {
            (fs - 1, le)
        } else {
            (0, le)
        };
        if end == start {
            return 0;
        }
        let list = EditList::new(vec![Edit::delete(start, end - start)])
            .expect("a single line-range delete is well-formed");
        let txn = Transaction::new(self.doc.revision(), list, TransactionOrigin::UserInput)
            .with_hint(GroupHint::BreakBefore);
        self.doc
            .apply(txn)
            .expect("line-range delete applies cleanly");
        self.view.last_was_edit = true;
        let nb = self.doc.bytes();
        self.view.cursor = start.min(nb.len());
        last - first + 1
    }

    /// `:[range]y` — yank the range's lines LINEWISE into the unnamed register (and `"0`), like `yy` over
    /// the span (no range = the cursor's line). Returns the number of lines yanked. Non-destructive: the
    /// buffer and cursor are unchanged (Vim moves the cursor to the range's last line; deferred).
    pub fn yank_lines(&mut self, range: SubRange) -> usize {
        let bytes = self.doc.bytes();
        let Ok(hay) = std::str::from_utf8(bytes) else {
            return 0;
        };
        let lines = line_spans(hay);
        let cursor_line = crate::pos::line_of(hay.as_bytes(), self.view.cursor);
        let (first, last) = resolve_line_range(range, &lines, cursor_line);
        let (fs, _) = lines[first];
        let (_, le) = lines[last];
        let end = if le < hay.len() { le + 1 } else { le };
        // `Register::linewise` normalizes to end with `\n`, so an unterminated last line still yanks clean.
        let text = bytes[fs..end].to_vec();
        self.view.registers.yank(None, Register::linewise(text));
        last - first + 1
    }

    /// `:[range]m {addr}` — move the range's lines to after the destination line, as one undo group.
    /// Returns the number of lines moved, or `None` if the destination lies inside the source (Vim's
    /// "move lines into themselves") or the buffer is not UTF-8. No range = the cursor's line.
    pub fn move_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        self.relocate_lines(range, dest, false)
    }

    /// `:[range]t {addr}` / `:copy` — COPY the range's lines to after the destination line (the source
    /// stays), as one undo group. Returns the number of lines copied, or `None` if the buffer is not UTF-8.
    pub fn copy_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        self.relocate_lines(range, dest, true)
    }

    /// Shared engine for `:m` (move) and `:t`/`:copy` (`copy = true` keeps the source). Rebuilds the line
    /// list and replaces the whole buffer in one transaction — simple and always correct for these
    /// non-hot ex commands (a surgical minimal-region edit is a possible later optimisation).
    fn relocate_lines(&mut self, range: SubRange, dest: LineAddr, copy: bool) -> Option<usize> {
        let bytes = self.doc.bytes();
        let hay = std::str::from_utf8(bytes).ok()?;
        let had_trailing_nl = hay.ends_with('\n');
        let mut lines: Vec<String> = if hay.is_empty() {
            Vec::new()
        } else {
            let mut v: Vec<String> = hay.split('\n').map(str::to_string).collect();
            if had_trailing_nl {
                v.pop(); // drop the empty element after the final '\n'
            }
            v
        };
        let nlines = lines.len();
        if nlines == 0 {
            return Some(0);
        }
        let spans = line_spans(hay);
        let cursor_line = crate::pos::line_of(hay.as_bytes(), self.view.cursor);
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        // `line_spans` counts a phantom empty line after a trailing '\n'; the split/pop `lines` vec does
        // not, so clamp to the vec.
        let last = last.min(nlines - 1);
        let first = first.min(last);
        // Destination as a 0-based insert index in `0..=nlines`.
        let ins = match dest {
            LineAddr::Line(n) => n.min(nlines),
            LineAddr::Last => nlines,
            LineAddr::Current => (cursor_line + 1).min(nlines),
        };
        // A move whose destination is inside/adjacent to the source is a no-op (Vim errors; we decline).
        if !copy && ins >= first && ins <= last + 1 {
            return None;
        }
        let block: Vec<String> = lines[first..=last].to_vec();
        let count = block.len();
        if !copy {
            lines.drain(first..=last);
        }
        // After a move removes the block, an insert index past the block shifts left by `count`.
        let adj = if copy || ins <= first {
            ins
        } else {
            ins - count
        }
        .min(lines.len());
        for (k, line) in block.into_iter().enumerate() {
            lines.insert(adj + k, line);
        }
        let mut text = lines.join("\n");
        if had_trailing_nl && !text.is_empty() {
            text.push('\n');
        }
        let new_bytes = text.into_bytes();
        // Cursor: onto the first relocated line at its new position.
        let cursor = new_bytes
            .split_inclusive(|&b| b == b'\n')
            .take(adj)
            .map(<[u8]>::len)
            .sum::<usize>()
            .min(new_bytes.len());

        let list = EditList::new(vec![Edit::replace(0, bytes.len(), new_bytes)])
            .expect("a single whole-buffer replace is well-formed");
        let txn = Transaction::new(self.doc.revision(), list, TransactionOrigin::UserInput)
            .with_hint(GroupHint::BreakBefore);
        self.doc.apply(txn).expect("line relocate applies cleanly");
        self.view.last_was_edit = true;
        self.view.cursor = cursor.min(self.doc.bytes().len());
        Some(count)
    }

    /// `:[range]sort[!] [i][n][r][u] [/pattern/]` — sort the range's lines (whole file when the caller
    /// passes `WholeFile`) as one undo group. The sort KEY per line is, in order: the text after the first
    /// `pattern` match (or the matched text itself under `r`), else the whole line; lowercased under `i`
    /// (`ignore_case`); its first decimal number under `n` (`numeric`). `reverse` (`!`) sorts descending;
    /// `unique` (`u`) drops runs of equal KEYS after sorting. The sort is stable. Returns lines removed by
    /// `u`.
    pub fn sort_lines(&mut self, range: SubRange, opts: &SortOptions) -> usize {
        let bytes = self.doc.bytes();
        let Ok(hay) = std::str::from_utf8(bytes) else {
            return 0;
        };
        let had_trailing_nl = hay.ends_with('\n');
        let mut lines: Vec<String> = if hay.is_empty() {
            Vec::new()
        } else {
            let mut v: Vec<String> = hay.split('\n').map(str::to_string).collect();
            if had_trailing_nl {
                v.pop();
            }
            v
        };
        if lines.is_empty() {
            return 0;
        }
        let spans = line_spans(hay);
        let cursor_line = crate::pos::line_of(hay.as_bytes(), self.view.cursor);
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        // `line_spans` counts a phantom empty line after a trailing '\n'; the split/pop `lines` vec does
        // not, so clamp the resolved indices to the vec.
        let last = last.min(lines.len() - 1);
        let first = first.min(last);

        // The comparable key for one line, honoring `/pattern/`, `r`, `i`, and `n`.
        let compiled = opts.pattern.as_deref().and_then(|p| {
            crate::pattern::Regex::compile(p, crate::pattern::Options::default()).ok()
        });
        let key_of = |line: &str| -> SortKey {
            // Slice the pattern-selected base: text after the match, or the match itself under `r`; a line
            // the pattern does not match yields an empty base (Vim floats non-matching lines to the front).
            let base: &str = match &compiled {
                Some(re) => match re.find_at(line, 0) {
                    Some(m) if opts.use_match => &line[m.start..m.end],
                    Some(m) => &line[m.end..],
                    None => "",
                },
                None => line,
            };
            if opts.numeric {
                SortKey::Num(first_number(base))
            } else if opts.ignore_case {
                SortKey::Text(base.to_lowercase())
            } else {
                SortKey::Text(base.to_string())
            }
        };

        let mut keyed: Vec<(SortKey, String)> = lines[first..=last]
            .iter()
            .map(|l| (key_of(l), l.clone()))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0)); // stable (Vim's sort is stable)
        if opts.unique {
            keyed.dedup_by(|a, b| a.0 == b.0);
        }
        if opts.reverse {
            keyed.reverse();
        }
        let seg: Vec<String> = keyed.into_iter().map(|(_, l)| l).collect();
        let removed = (last - first + 1) - seg.len();
        lines.splice(first..=last, seg);

        let mut text = lines.join("\n");
        if had_trailing_nl && !text.is_empty() {
            text.push('\n');
        }
        let new_bytes = text.into_bytes();
        let cursor = new_bytes
            .split_inclusive(|&b| b == b'\n')
            .take(first)
            .map(<[u8]>::len)
            .sum::<usize>()
            .min(new_bytes.len());
        let list = EditList::new(vec![Edit::replace(0, bytes.len(), new_bytes)])
            .expect("a single whole-buffer replace is well-formed");
        let txn = Transaction::new(self.doc.revision(), list, TransactionOrigin::UserInput)
            .with_hint(GroupHint::BreakBefore);
        self.doc.apply(txn).expect("sort applies cleanly");
        self.view.last_was_edit = true;
        self.view.cursor = cursor.min(self.doc.bytes().len());
        removed
    }

    /// Apply a set of DISJOINT byte-range replacements as ONE undo group, tagged with the caller-supplied
    /// `origin` — a separate undo unit from user edits (F-005). This is a PROVENANCE-AGNOSTIC batch-edit
    /// primitive: the CORE provides the mechanism, the CALLER states the policy (the LSP frontend passes
    /// [`TransactionOrigin::Lsp`]; a snippet/macro/AI batch would pass its own origin). Each edit is
    /// `(start, end, replacement)`; out-of-range or overlapping sets are skipped (formatter output is disjoint).
    /// The cursor is clamped into the new buffer. No-op on an empty/invalid set.
    pub fn apply_edits(&mut self, edits: &[(usize, usize, String)], origin: TransactionOrigin) {
        let len = self.doc.bytes().len();
        let mut valid: Vec<(usize, usize, String)> = edits
            .iter()
            .filter(|(s, e, _)| s <= e && *e <= len)
            .cloned()
            .collect();
        if valid.is_empty() {
            return;
        }
        valid.sort_by_key(|(s, _, _)| *s); // EditList requires ascending, disjoint edits
        let es: Vec<Edit> = valid
            .into_iter()
            .map(|(s, e, t)| Edit::replace(s, e - s, t.into_bytes()))
            .collect();
        let Ok(list) = EditList::new(es) else {
            return; // overlapping edits — refuse rather than corrupt the buffer
        };
        let txn =
            Transaction::new(self.doc.revision(), list, origin).with_hint(GroupHint::BreakBefore);
        if self.doc.apply(txn).is_ok() {
            self.view.last_was_edit = true;
            let clamped = self.view.cursor.min(self.doc.bytes().len());
            self.set_cursor(clamped);
        }
    }

    /// One indent level as bytes: `tab_width` spaces (space style) or a single `\t` (tab style).
    fn indent_unit(&self) -> Vec<u8> {
        match self.view.indent.style {
            IndentStyle::Space => vec![b' '; self.view.indent.tab_width],
            IndentStyle::Tab => vec![b'\t'],
        }
    }

    /// The unnamed register's current contents (for tests / a future `:registers`).
    #[must_use]
    pub fn register(&self) -> &Register {
        self.view.registers.unnamed()
    }

    /// The whole register store (for tests / a future `:registers`), giving access to the named slots.
    #[must_use]
    pub fn registers(&self) -> &RegisterStore {
        &self.view.registers
    }

    /// The one-shot register selected by the most recent `"x` and not yet consumed. The
    /// [`Workspace`](crate::Workspace) reads this before applying a command to decide whether that command
    /// touches the system clipboard (`"+`/`"*`).
    #[must_use]
    pub fn pending_register(&self) -> Option<char> {
        self.view.pending_register
    }

    /// Refresh the `"+`/`"*` clipboard mirror slot from external OS-clipboard bytes ahead of a paste, so
    /// `"+p` reflects whatever another application copied. Preserves the in-session paste geometry when the
    /// bytes are unchanged (see [`RegisterStore::set_clipboard_from_external`]).
    pub fn sync_clipboard_in(&mut self, bytes: Vec<u8>) {
        self.view.registers.set_clipboard_from_external(bytes);
    }

    /// Write raw bytes into a named register as a CHARWISE entry (D-055 macro recording): the frontend
    /// stores a recorded keystroke stream into `"{name}`, sharing the same a-z slots as yank/paste so a
    /// macro pastes as text and yanked text runs as a macro. `name` is `Some('a'..='z')`; the byte content
    /// is opaque to the core (it is a key stream, not display text).
    pub fn set_register_raw(&mut self, name: Option<char>, bytes: Vec<u8>) {
        self.view
            .registers
            .set_macro(name, crate::register::Register::charwise(bytes));
    }

    /// The highlighted byte range `[start, end)` of the current charwise/linewise Visual selection, or
    /// `None` in Normal/Insert **and for a blockwise selection** (a rectangle is not one contiguous range —
    /// use [`EditorState::block_spans`] to paint that). Charwise includes the character under the active
    /// end; linewise spans whole lines. For the frontend to paint the selection.
    #[must_use]
    pub fn selection_span(&self) -> Option<(usize, usize)> {
        // Visual and Select paint the same selection (they share the anchor and toggle via CTRL-G).
        self.view.selection_span(self.bytes())
    }

    /// The per-row highlighted byte ranges of the current BLOCKWISE selection (one `[start, end)` per line
    /// the rectangle crosses; short lines contribute an empty range at their end), or `None` when the
    /// selection is not blockwise. For the frontend to paint a column-aligned block.
    #[must_use]
    pub fn block_spans(&self) -> Option<Vec<(usize, usize)>> {
        self.view.block_spans(self.bytes())
    }

    /// The document bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.doc.bytes()
    }

    /// The document as UTF-8, or `None` if not valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        self.doc.as_str()
    }

    /// The cursor's byte offset (on a char boundary).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.view.cursor
    }

    /// The jumplist positions (oldest → newest) for `:jumps`.
    #[must_use]
    pub fn jumps_snapshot(&self) -> Vec<usize> {
        self.view.jumps.clone()
    }

    /// The change-list positions (oldest → newest) for `:changes`.
    #[must_use]
    pub fn changes_snapshot(&self) -> Vec<usize> {
        self.view.changes.clone()
    }

    /// The SET marks for `:marks` — the named marks `a`–`z`, the `.` last-change mark, and the `^`
    /// last-insert mark — as `(name, byte offset)`, in that order (unset marks omitted).
    #[must_use]
    pub fn marks_snapshot(&self) -> Vec<(char, usize)> {
        let mut out = Vec::new();
        for c in 'a'..='z' {
            if let Some(p) = self.view.named_mark(c) {
                out.push((c, p));
            }
        }
        if let Some(p) = self.view.last_change() {
            out.push(('.', p));
        }
        if let Some(p) = self.view.last_insert() {
            out.push(('^', p));
        }
        out
    }

    /// The mark's byte offset, or `None` when no mark is set. The mark is the Emacs profile's other end of
    /// the region (`set-mark-command` and the region ops); one-caret degenerate is `None`. View-local. For
    /// the Emacs parity comparator and a future `:marks`.
    #[must_use]
    pub fn mark(&self) -> Option<usize> {
        self.view.mark
    }

    /// Place the cursor at byte offset `pos` (on a char boundary). View-local; does not touch the document.
    /// Used to home a fixture's starting point in the parity comparator (mirrors the frontend seam).
    ///
    /// Placing the caret also SEEDS the sticky desired column (`curswant`) to `pos`'s column, exactly as a
    /// committed motion would: a subsequent `j`/`k` (`next-line`/`previous-line`) aims at the column the
    /// caret was placed on, not column 0. Without this a teleport (goto, mouse, a fixture's start point)
    /// would leave `curswant` stale at 0 and vertical moves would snap to the line start.
    pub fn set_cursor(&mut self, pos: usize) {
        let b = self.doc.bytes();
        // Clamp to the buffer and snap to a char boundary: `set_cursor` is a public seam (tests, a
        // future goto/mouse), so an out-of-range or mid-codepoint `pos` must not leave the cursor where
        // a later `col_of`/slice would panic. Vim never overshoots, so this is a no-op on valid input.
        let pos = snap(b, pos.min(b.len()));
        self.view.cursor = pos;
        self.view.curswant = col_of(b, line_start(b, pos), pos);
    }

    /// This View's caret gravity (D-050): `OnChar` (Vim/Neovim) or `BetweenChar` (Emacs profile).
    #[must_use]
    pub fn caret_gravity(&self) -> CaretGravity {
        self.view.caret
    }

    /// Select this View's caret gravity (D-050 / RFC-0015). The Emacs profile sets `BetweenChar` so its
    /// edits are not Vim-clamped; Vim/Neovim leave the `OnChar` default. Profile-init seam.
    pub fn set_caret_gravity(&mut self, gravity: CaretGravity) {
        self.view.caret = gravity;
    }

    /// The current mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.view.mode
    }

    /// Whether the buffer differs from what is on disk (delegates to the document's node-identity check).
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.doc.is_modified()
    }
}

mod range;
pub(crate) use range::*;

mod search;
use search::{match_spans, search_bwd, search_fwd};

mod substitute;
/// The options a `:sort` carries (parsed by the frontend from `[!][i][n][r][u] [/pattern/]`). Grouped into
/// one struct so [`EditorState::sort_lines`] and the `Workspace` wrapper take a single argument.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct SortOptions {
    /// `!` — sort descending.
    pub reverse: bool,
    /// `n` — sort on each line's first decimal number (in the pattern-selected key).
    pub numeric: bool,
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

/// A line's comparable sort key: a number (`n`) or text. All keys in one sort share a variant, so the
/// derived ordering (which compares `Num` < `Text` across variants) never actually mixes them.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Num(i64),
    Text(String),
}

/// The first decimal number in `s` (with an optional leading `-`), for `:sort n`. Lines without a number
/// sort as `0` (Vim keeps them before the numbered lines, which a stable sort preserves).
fn first_number(s: &str) -> i64 {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i].is_ascii_digit() {
            let start = if i > 0 && b[i - 1] == b'-' { i - 1 } else { i };
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            return s[start..j].parse::<i64>().unwrap_or(0);
        }
    }
    0
}

pub(crate) use substitute::{expand_replacement, line_spans, resolve_line_range};
pub use substitute::{GlobalCmd, LineAddr, SubFlags, SubOutcome, SubRange, Substitution};

mod planner;
pub use planner::plan;

/// Apply a plan to the state, returning the effects the frontend must perform.
/// The `(start, end)` byte span (half-open, POST-edit coordinates) the `[`/`]` change marks bracket for an
/// [`EditList`]: `start` is the first edit's position (nothing before it moved), and `end` is the end of the
/// LAST edit's inserted text — its position shifted by the cumulative length delta of every earlier edit,
/// plus that edit's inserted byte count. A pure delete yields `end == start` (both marks collapse onto the
/// deletion point, as Vim does). `None` for an empty list.
fn edit_bracket(list: &EditList) -> Option<(usize, usize)> {
    let edits = list.edits();
    let first = edits.first()?;
    let last = edits.last()?;
    let start = first.pos.0;
    let delta_before_last: isize = edits[..edits.len() - 1].iter().map(Edit::delta).sum();
    let post_last_start = (last.pos.0 as isize + delta_before_last).max(0) as usize;
    Some((start, post_last_start + last.ins.len()))
}

/// Whether `mode` is one of the text-entry sessions (Insert / Replace / VirtualReplace) — used to decide how
/// the `[`/`]` change marks are bounded (a whole typed run, tracked from session entry to exit) versus a
/// one-shot Normal edit (bounded directly by its [`EditList`]).
fn is_text_entry(mode: Mode) -> bool {
    matches!(mode, Mode::Insert | Mode::Replace | Mode::VirtualReplace)
}

pub fn commit(st: &mut EditorState, plan: Plan) -> Vec<Effect> {
    // `"x` sets the one-shot pending register and nothing else — it must NOT be cleared by the tail below
    // (that clear is what consumes the selection on the FOLLOWING command), so it returns early.
    if let Action::SetPending(name) = plan.action {
        st.view.pending_register = name;
        return plan.effects;
    }
    let entry_selection = st.view.mode.selection();
    let was_selection = entry_selection.is_some();
    let entry_cursor = st.view.cursor;
    let entry_anchor = st.view.anchor;
    let entry_mode = st.view.mode;
    // The `[`/`]` change-mark span for a Normal-mode edit — read off the plan's edits (post-edit coords)
    // before they are moved into a Transaction. A yank has no edits; its span rides in `RegWrite::Yank`.
    let mut edit_span: Option<(usize, usize)> = None;
    // `g;`/`g,` resolve their destination here (they step `change_idx`, which the pure planner cannot do).
    let mut nav_target: Option<usize> = None;
    match plan.action {
        Action::Txn { edits, hint } => {
            edit_span = edit_bracket(&edits);
            let txn = Transaction::new(st.doc.revision(), edits, TransactionOrigin::UserInput)
                .with_hint(hint);
            // The plan built the edits from the current buffer, so apply cannot be stale or out of range.
            st.doc
                .apply(txn)
                .expect("planned transaction applies cleanly");
        }
        Action::ReplaceTxn {
            edits,
            hint,
            push,
            pop,
        } => {
            edit_span = edit_bracket(&edits);
            let txn = Transaction::new(st.doc.revision(), edits, TransactionOrigin::UserInput)
                .with_hint(hint);
            st.doc
                .apply(txn)
                .expect("planned transaction applies cleanly");
            if pop {
                st.view.replace_stack.pop();
            }
            if let Some(entry) = push {
                st.view.replace_stack.push(entry);
            }
        }
        Action::Undo => {
            st.doc.undo();
        }
        Action::Redo => {
            st.doc.redo();
        }
        Action::UndoChrono { older } => {
            st.doc.undo_chronological(older);
        }
        Action::JumpChange { older } => {
            nav_target = st.view.nav_change(older);
        }
        Action::JumpList { older } => {
            nav_target = st.view.nav_jump(older, entry_cursor);
        }
        Action::SetNamedMark { ch } => {
            st.view.set_named_mark(ch, st.view.cursor);
        }
        Action::BlockInsertArm {
            edits,
            hint,
            session,
        } => {
            if !edits.is_empty() {
                edit_span = edit_bracket(&edits);
                let txn = Transaction::new(st.doc.revision(), edits, TransactionOrigin::UserInput)
                    .with_hint(hint);
                st.doc
                    .apply(txn)
                    .expect("planned transaction applies cleanly");
            }
            st.view.block_insert = Some(session);
        }
        Action::Nop => {}
        // Handled by the early return above; the buffer-mutating tail never runs for it.
        Action::SetPending(_) => unreachable!("SetPending is handled before the action match"),
    }
    // The cursor the plan computed is valid for the post-action buffer, except undo/redo which resize the
    // text unpredictably — clamp and snap to a char boundary either way.
    // `g;`/`g,` navigate the change list: the action stepped `change_idx` above and yielded the target
    // (or `None` at an end), which overrides the plan's placeholder cursor for this frame only.
    let resolved_cursor = nav_target.unwrap_or(plan.cursor);
    st.view.cursor = snap(st.doc.bytes(), resolved_cursor);
    // Vim `` `^ ``: leaving Insert for Normal records where the caret was (the pre-clamp insert position),
    // so `gi` resumes Insert exactly there. Checked before the mode is overwritten below.
    if matches!(st.view.mode, Mode::Insert) && matches!(plan.mode, Mode::Normal) {
        st.view.last_insert = Some(entry_cursor);
    }
    st.view.mode = plan.mode;
    st.view.last_was_edit = plan.is_edit;
    // Vim change list: an edit records where it happened (the cursor it left behind) as the newest entry,
    // which is also the `` `. `` mark. Motions/undo/g;/g, don't push. Snapped below with the other offsets.
    if plan.is_edit {
        st.view.push_change(st.view.cursor);
    }
    // Vim `[`/`]` change marks: bound the last changed OR yanked text. A text-entry session (Insert/Replace)
    // is ONE change bracketed from where it began to the caret at exit (so `ihello<Esc>` brackets the whole
    // run, not just the last typed char); a one-shot Normal edit is bounded by its `EditList`; a yank by the
    // span carried in `RegWrite::Yank`. `]` is stored as the LAST char (inclusive) for delete/replace/put/
    // yank, but as the exclusive insert END-caret for a session — the jump's EOL clamp reconciles both to
    // match Neovim. Individual keystrokes WITHIN a session don't touch the marks (only entry/exit do).
    let entering_entry = !is_text_entry(entry_mode) && is_text_entry(plan.mode);
    let leaving_entry = is_text_entry(entry_mode) && !is_text_entry(plan.mode);
    if entering_entry {
        st.view.insert_start = Some(st.view.cursor);
    }
    if leaving_entry {
        let start = st.view.insert_start.unwrap_or(entry_cursor);
        st.view.change_start = Some(start.min(entry_cursor));
        st.view.change_end = Some(start.max(entry_cursor));
        st.view.insert_start = None;
    } else if !is_text_entry(plan.mode) {
        // A completed Normal-mode edit, or a yank (which carries its own span since it makes no edits).
        let yank_span = match &plan.set_register {
            Some(RegWrite::Yank(_, span)) => Some(*span),
            _ => None,
        };
        if let Some((s, e)) = edit_span.or(yank_span) {
            let b = st.doc.bytes();
            let s = s.min(b.len());
            st.view.change_start = Some(s);
            st.view.change_end = Some(if e > s {
                prev_grapheme(b, e.min(b.len()))
            } else {
                s
            });
        }
    }
    let len = st.doc.bytes().len();
    for c in st.view.changes.iter_mut() {
        *c = snap(st.doc.bytes(), (*c).min(len));
    }
    // Named marks (`m{a-z}`) snap into range too, so an edit that shrank the buffer under a mark can never
    // make a later `` `{a-z} `` jump out of bounds.
    for m in st.view.named_marks.iter_mut().flatten() {
        *m = snap(st.doc.bytes(), (*m).min(len));
    }
    // The last-insert position (`gi`) snaps the same way.
    if let Some(li) = st.view.last_insert {
        st.view.last_insert = Some(snap(st.doc.bytes(), li.min(len)));
    }
    // The `[`/`]` change marks (and an in-flight Insert-session start) snap into range too.
    for p in [
        &mut st.view.change_start,
        &mut st.view.change_end,
        &mut st.view.insert_start,
    ]
    .into_iter()
    .flatten()
    {
        *p = snap(st.doc.bytes(), (*p).min(len));
    }
    // Jumplist entries snap into range too (a buffer-resizing edit under a jump keeps it valid).
    for j in st.view.jumps.iter_mut() {
        *j = snap(st.doc.bytes(), (*j).min(len));
    }
    // The `<BS>`-restore history lives only while a replace session is active; drop it on any exit.
    if !matches!(st.view.mode, Mode::Replace | Mode::VirtualReplace) {
        st.view.replace_stack.clear();
    }
    // A blockwise insert-replicate session lives only while Insert is held; drop it on any exit (the
    // `<Esc>` that closes it already read the session to build the replicate before this runs).
    if st.view.mode != Mode::Insert {
        st.view.block_insert = None;
    }
    // Vim never rests the Normal-mode cursor on the newline: after an edit that leaves it beyond the final
    // char of a non-empty line, pull it back onto the last char (e.g. `dw` on the last word → the cursor
    // clamps to the trailing char rather than the line end). Scoped to edits in Normal mode so it never
    // touches Insert's legitimate cursor-past-end, and guarded by `ls < le` so an empty line keeps `[n,0]`.
    // Gated on `CaretGravity::OnChar` (D-050): the Emacs profile is BetweenChar, so Emacs point legitimately
    // rests on the after-last slot and must NOT be clamped (`kill-line`/`yank` land point N+1, not N).
    if plan.is_edit && st.view.mode == Mode::Normal && st.view.caret == CaretGravity::OnChar {
        let b = st.doc.bytes();
        let le = line_end(b, st.view.cursor);
        let ls = line_start(b, st.view.cursor);
        if st.view.cursor == le && ls < le {
            st.view.cursor = prev_boundary(b, le);
        }
    }
    // A yank/delete/change writes its captured span into the pending register (or unnamed when none),
    // mirroring the unnamed slot on a named write; append (`"A`) is handled inside the store. An Emacs kill
    // additionally ACCUMULATES onto the unnamed entry when the previous command was also a kill, and always
    // marks the run so the next kill accumulates onto it; any other command breaks the run.
    let was_kill = st.view.last_was_kill;
    st.view.last_was_kill = false;
    match plan.set_register {
        Some(RegWrite::Edit(reg)) => st.view.registers.delete(st.view.pending_register, reg),
        Some(RegWrite::Yank(reg, _)) => st.view.registers.yank(st.view.pending_register, reg),
        Some(RegWrite::KillAppend(reg)) => {
            if was_kill {
                st.view.registers.kill_append(reg);
            } else {
                st.view.registers.write(st.view.pending_register, reg);
            }
            st.view.last_was_kill = true;
        }
        Some(RegWrite::KillPrepend(reg)) => {
            if was_kill {
                st.view.registers.kill_prepend(reg);
            } else {
                st.view.registers.write(st.view.pending_register, reg);
            }
            st.view.last_was_kill = true;
        }
        None => {}
    }
    // Maintain the selection anchor: set it when entering a selection mode (Visual/Select; the fixed end
    // is where the cursor was), keep it while staying in one — including across a Visual↔Select CTRL-G
    // toggle, since both are selection modes — and clear it on any exit to Normal/Insert.
    match (was_selection, st.view.mode.selection().is_some()) {
        (false, true) => st.view.anchor = Some(entry_cursor),
        (_, false) => {
            // Leaving a selection: remember it (anchor, active end, kind) for `gv` BEFORE dropping the
            // anchor — the depth-1 slice of D-027's `` `< ``/`` `> `` history. Only fires on an actual exit
            // (entry_selection is None when we were already in Normal), so a plain Normal command is inert.
            if let (Some(kind), Some(a)) = (entry_selection, entry_anchor) {
                st.view.last_visual = Some((a, entry_cursor, kind));
            }
            st.view.anchor = None;
        }
        (true, true) => {}
    }
    // A text object in a selection mode overrides the anchor to span the object (both ends move at once).
    if let Some(a) = plan.set_anchor {
        st.view.anchor = Some(a);
    }
    // Keep the raw-offset anchor valid: an edit applied while in Visual mode can resize the buffer under it,
    // and a stale anchor past the new end would make `selection_range` slice out of bounds (a core panic).
    // Snapping clamps it into range and onto a char boundary. The edit-tracking anchor-store position that
    // would move the anchor *semantically* with the edit is deferred (D-027); v0 only guarantees totality.
    if let Some(a) = st.view.anchor {
        st.view.anchor = Some(snap(st.doc.bytes(), a));
    }
    // The Emacs mark (D-027): a plan installs or drops it; otherwise it persists. Snap it into range like the
    // anchor so an edit that resized the buffer under it can never make `KillRegion` slice out of bounds.
    match plan.set_mark {
        Some(MarkWrite::Set(m)) => st.view.mark = Some(m),
        None => {}
    }
    if let Some(m) = st.view.mark {
        st.view.mark = Some(snap(st.doc.bytes(), m));
    }
    // The pending register (`"x`) is one-shot: any command other than `SetRegister` (which returned early
    // above) consumes it. Cleared here AFTER the register write so a stray `"x` before a non-register
    // command is simply forgotten, and a later plain edit never leaks into the named slot.
    st.view.pending_register = None;
    plan.effects
}

/// Whether `cmd` is a Vim JUMP — a command that sets the `''`/jumplist "before" mark (search, `G`/`gg`, `%`,
/// a mark jump, paragraph/sentence motion). A plain `h`/`j`/`k`/`l`/word motion is NOT a jump. `CTRL-O`/
/// `CTRL-I` themselves are excluded so navigating the jumplist does not record new entries.
fn is_jump(cmd: &Command) -> bool {
    use crate::motion::Motion as M;
    match cmd {
        Command::SearchNext(_) | Command::SearchPrev(_) | Command::SearchWordUnder { .. } => true,
        Command::GotoLastChange
        | Command::GotoLastChangeLine
        | Command::GotoChangeMarkStart
        | Command::GotoChangeMarkEnd
        | Command::GotoChangeMarkStartLine
        | Command::GotoChangeMarkEndLine
        | Command::GotoNamedMark(_)
        | Command::GotoNamedMarkLine(_)
        | Command::GotoContextMark
        | Command::GotoContextMarkLine => true,
        Command::Move(_, m) => matches!(
            m,
            M::GotoLine
                | M::LastLine
                | M::MatchBracket
                | M::GotoPercent
                | M::ParagraphFwd
                | M::ParagraphBack
                | M::SentenceFwd
                | M::SentenceBack
        ),
        _ => false,
    }
}

/// Convenience: plan then commit one command, then maintain the sticky desired column (curswant).
pub fn apply_command(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    // A jump records the position it LEAVES onto the jumplist (Vim), so `CTRL-O` can return there.
    let jump_from = is_jump(cmd).then_some(st.view.cursor);
    let p = plan(st, cmd);
    let effects = commit(st, p);
    if let Some(from) = jump_from {
        st.view.push_jump(from);
    }
    // Maintain the auto-indent-pending flag (read by `EnterNormal` for `<Esc>` autoindent cleanup): a
    // tree-suggested open sets it; typing a non-blank clears it; any Insert entry/exit resets it so it
    // never leaks to a later line. Done here (not in the pure planner) because it depends on the Command.
    match cmd {
        Command::OpenLineIndent { level, .. } => st.view.auto_indent_pending = *level > 0,
        Command::InsertChar(c) if *c != ' ' && *c != '\t' => st.view.auto_indent_pending = false,
        Command::EnterNormal
        | Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::AppendLineEnd
        | Command::OpenBelow
        | Command::OpenAbove => st.view.auto_indent_pending = false,
        _ => {}
    }
    update_curswant(st, cmd);
    effects
}

/// Maintain [`EditorState::curswant`] after a command (Vim's curswant rule): a vertical move KEEPS the
/// wanted column (ride it through short lines); `$`/`<End>`/`A` (append) set it to [`MAXCOL`] so subsequent
/// `j`/`k` — and an Insert caret — stay at the line's end; every other command recomputes it from the
/// cursor's new char column.
///
/// Insert-mode append column (Vim `i_CTRL-O` at EOL): while `curswant == MAXCOL` in Insert, the caret rests
/// at the line's END (the append position), and a one-shot `CTRL-O` command that is not itself a column-
/// setting move (e.g. `dd`) PRESERVES that append intent rather than recomputing it — so `A<C-o>ddX` appends
/// `X` at the end of the line dd left behind, and `i<C-o>$X` appends at end. Called from [`apply_command`],
/// the single plan+commit driver, so `cmd` is always in scope.
fn update_curswant(st: &mut EditorState, cmd: &Command) {
    let insert = matches!(st.view.mode, Mode::Insert);
    match cmd {
        Command::Move(_, Motion::Up)
        | Command::Move(_, Motion::Down)
        | Command::MoveUp
        | Command::MoveDown => {} // keep the sticky column
        // `$`/`<End>` and `A` (append) want the line's end.
        Command::Move(_, Motion::LineEnd) | Command::MoveLineEnd | Command::AppendLineEnd => {
            st.view.curswant = MAXCOL
        }
        // Commands that establish a definite column: horizontal moves, the Insert-native keys, and every
        // insert-ENTRY except `A` (which is the append/MAXCOL case above) reset the wanted column — they
        // never preserve a stale append intent from an earlier `$`/`A`.
        Command::Move(_, _)
        | Command::MoveLeft
        | Command::MoveRight
        | Command::MoveLineStart
        | Command::InsertChar(_)
        | Command::InsertNewline
        | Command::DeleteBack
        | Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::OpenBelow
        | Command::OpenAbove
        | Command::EnterReplace
        | Command::EnterVirtualReplace
        // A `/`/`?` search is a definite motion/edit: it establishes the match column (and, for `c?`,
        // the deletion point where Insert begins). It must never preserve a stale `$`/`A` append intent
        // — `$c?pat` inserts at the match, not at the line end.
        | Command::Search { .. } => {
            let b = st.doc.bytes();
            st.view.curswant = col_of(b, line_start(b, st.view.cursor), st.view.cursor);
        }
        // Everything else (edits like `dd`/`x`, mode changes): in Insert with an append intent (a one-shot
        // `CTRL-O` command run while `curswant == MAXCOL`), PRESERVE MAXCOL; otherwise recompute the column.
        _ => {
            if !(insert && st.view.curswant == MAXCOL) {
                let b = st.doc.bytes();
                st.view.curswant = col_of(b, line_start(b, st.view.cursor), st.view.cursor);
            }
        }
    }
    // In Insert, a MAXCOL wanted column parks the caret at the append position (end of line).
    if insert && st.view.curswant == MAXCOL {
        let b = st.doc.bytes();
        st.view.cursor = line_end(b, st.view.cursor);
    }
}

#[cfg(test)]
mod tests;
