//! The editor state and the pure **plan / commit** command pipeline (RFC-0012).
//!
//! [`plan`] is a *pure decision*: `(&EditorState, &Command) -> Plan`, no mutation, no IO. [`commit`] applies
//! a `Plan` and returns the [`Effect`]s the frontend must perform. Because the core never does IO, replaying
//! the same commands on the same initial document is deterministic (see [`crate::trace`]). This is the split
//! that captures most of a Haskell rewrite's benefit in Rust — enforced by an empty dependency set.

use crate::anchor::Tracked;
use crate::cells::{Tabs, VarStops};
use crate::command::SelectKind;
use crate::document::{Document, DocumentId, TxnError};
use crate::edit::{Edit, EditList};
use crate::motion::snap;
use crate::pos::BytePos;
use crate::register::RegisterStore;
use crate::transaction::{GroupHint, Transaction, TransactionOrigin};
// The test suites under `tests/` read these through `use crate::editor::*`.
#[cfg(test)]
use crate::{
    command::{BlockInsertKind, Command, ForcedWise, OpKind, OpenKind, SearchOffset, WordCase},
    effect::Effect,
    motion::{self, Motion},
};

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
    /// Vim's window-local 'scroll' (the `CTRL-D`/`CTRL-U` amount) as `(lines, height)`: a count given to
    /// `CTRL-D`/`CTRL-U` sets it, and it holds only while the window keeps the height it was set at (Vim
    /// recomputes 'scroll' as half the height on a resize). `None` = half the window height. Frontend-owned,
    /// like `top`.
    scroll: Option<(usize, usize)>,
    /// This window's own `'scrolloff'` (#1151), or `None` to use the global value (nvim's local `-1`).
    /// Frontend-owned, like `top`: the viewport pass reads it, and a split copies it with the View.
    scrolloff: Option<i64>,
    /// The first displayed screen column when `'wrap'` is off (nvim's `w_leftcol`). Frontend-owned, like
    /// `top`: the viewport pass and `zh`/`zl`/`zs`/`ze` set it (#615).
    leftcol: usize,
    /// The virtual columns of the top line scrolled out of the window (nvim's `w_skipcol`):
    /// `'smoothscroll'` scrolling, or a cursor line taller than the window. Frontend-owned, like `top`
    /// (#615).
    skipcol: usize,
    /// nvim's window-local display options (#615, #616). The core only stores them: the frontend lays
    /// out with them, and a split copies them with the View.
    window: crate::WindowOptions,
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
    /// degenerate of D-027's `` `< ``/`` `> `` selection history: one remembered selection, each end a
    /// [`Mark`] (the offset, and nvim's raw column past it, #1304).
    last_visual: Option<(Mark, Mark, SelectKind)>,
    /// The kind `visualmode()` reports (nvim's `b_visual_mode_eval`): the kind of the last Visual/Select
    /// exit, set beside [`View::last_visual`], except that the uppercase line forms `D`/`X`/`Y`/`C`/`S`/`R`
    /// over a charwise or blockwise selection report linewise while `gv` and `'<`/`'>` keep the selection's
    /// own kind and columns (nvim's `v_visop` sets `VIsual_mode = 'V'` and restores the area's, #1305).
    /// `None` until a Visual area has been left.
    visual_mode_eval: Option<SelectKind>,
    /// Replace-mode (`R`) session history for `<BS>` restore: one entry per key typed, in order. `Some(orig)`
    /// = that key OVERWROTE a char (its original bytes, to restore on backspace); `None` = it APPENDED at
    /// end-of-line (backspace deletes it). Empty outside a Replace session; cleared on leaving Replace.
    replace_stack: Vec<Option<Vec<u8>>>,
    /// A live blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`), armed by [`Action::BlockInsertArm`]
    /// and consumed by the next `<Esc>` ([`Command::EnterNormal`]). `None` outside such a session; cleared
    /// on any exit from Insert. See [`BlockInsert`].
    block_insert: Option<BlockInsert>,
    /// The geometry of the LAST committed blockwise insert-replicate — retained so dot-repeat (`.`) can
    /// rebuild an equivalent block at the caret when there is no live selection. nvim repeats a block
    /// `I`/`A`/`c` over the SAME width x height, positioned at the new cursor (verified vs nvim v0.12.4); the
    /// input engine holds no buffer geometry, so it lives here and the replayed [`Command::BlockInsert`]
    /// (issued with no anchor on `.`) reads it. `None` until the first block insert; survives leaving Insert
    /// (unlike [`Self::block_insert`]) so it is available to a later `.`. See [`BlockGeom`].
    last_block_geom: Option<BlockGeom>,
    /// The amount of text the last Visual-selection operator covered, for its dot-repeat (#794). See
    /// [`VisualOpGeom`]; set and read by [`prepare_selection_op`].
    last_visual_op: Option<VisualOpGeom>,
    /// Whether the current Insert session opened a line with AUTO-INDENT and nothing non-blank has been
    /// typed on it since (Vim). While true, leaving Insert (`<Esc>`) on an all-whitespace line removes the
    /// auto-inserted indent so `o<Esc>` never leaves trailing whitespace. Set/cleared in [`apply_command`]
    /// (which sees the `Command`); read by the `EnterNormal` planner arm. Cleared on any non-blank insert
    /// and on every Insert entry/exit, so it never survives to a later line.
    auto_indent_pending: bool,
    /// The `'smartindent'` rules armed on a line just opened, until the first key typed on it (#617).
    si_state: indent::SiState,
    /// The last character of the comment-end string a line opened inside a three-part comment may close
    /// with (nvim's `end_comment_pending`, the `x` flag of `'comments'`): typing it first on the line
    /// replaces the middle leader by the end one (` * /` → ` */`, #618).
    end_comment: Option<u8>,
    /// The indent `^ CTRL-D` deleted, which the next Insert `<CR>` gives back under `'autoindent'` (nvim's
    /// `old_indent`, #617). `0` when there is none.
    old_indent: usize,
    /// Whether the character before the caret is a space typed in this Insert (nvim's `inserted_space`),
    /// which a `'softtabstop'` `<BS>` deletes alone (#617).
    typed_space: bool,
    /// The indent config (`editor.tab_width` + `editor.indent_style`).
    indent: IndentConfig,
    /// `'textwidth'` (`tw`): the column `gq`/`gw` wrap to; `0` = use the 79-column fallback (Vim).
    text_width: usize,
    /// The text-formatting options (#618).
    format: FormatConfig,
    /// The columns of this View's text area (the window width less its gutter), set by the frontend:
    /// `'wrapmargin'` counts from its right edge and a `gq` with neither `'textwidth'` nor `'wrapmargin'`
    /// wraps one short of it (at most 79). 80 until the frontend says otherwise (a headless nvim's width).
    text_cols: usize,
    /// The STICKY desired column (Vim `curswant`): the char column `j`/`k` aim for, preserved across shorter
    /// interior lines rather than collapsing to the short line's end. Maintained in [`apply_command`] — kept
    /// on a vertical move, set to [`MAXCOL`] by `$`/`<End>` (ride each line's end), and recomputed from the
    /// landing column after any other command. Read by the `plan` Move Up/Down arm via [`motion::vmove`].
    curswant: Want,
    /// The search-case config (`'ignorecase'` + `'smartcase'`, F-009).
    search_case: SearchCase,
    /// Caret gravity (D-050): `OnChar` for Vim/Neovim (default), `BetweenChar` for the Emacs profile. Gates
    /// the Normal-mode on-character edit clamp in [`commit`] so Emacs point rests after the last char.
    caret: CaretGravity,
    /// The change list (Vim `:changes`): recent EDIT positions, oldest → newest, bounded to
    /// [`MAX_CHANGES`]. Its last entry is the `` `. `` mark. Kept in [`commit`] after any committing edit by
    /// Vim's rule (`changed_common`): an entry per undo step, not per keystroke — see [`View::push_change`] —
    /// and snapped so a later buffer-resizing edit keeps every entry in range. Navigated by `g;` / `g,`
    /// through [`change_idx`].
    changes: Vec<usize>,
    /// Vim's `b_new_change`: an undo step began since the change list last took a new entry, so the next
    /// change far enough from the newest entry takes one (#722).
    new_change: bool,
    /// The current cursor into [`changes`] for `g;`/`g,`. Equals `changes.len()` (past the newest) right
    /// after an edit, so the first `g;` lands on the newest change; walks toward 0 (oldest) on `g;` and back
    /// toward the newest on `g,`. Reset to `changes.len()` whenever a new change is pushed.
    change_idx: usize,
    /// The named marks `a`–`z`, `A`–`Z`, then `0`–`9` (Vim `m{a-zA-Z}` / `` `{a-zA-Z0-9} ``), indexed by
    /// [`mark_slot`]. `None` = unset. Set by [`Command::SetNamedMark`] (a numbered mark only by `:mark`,
    /// #1241), read by [`Command::GotoNamedMark`], and snapped on every commit like the change list. An
    /// uppercase or numbered (file) mark is kept with this view's buffer: a jump to one set in another
    /// buffer is deferred (#1088), and the numbered marks are not filled from a shada file.
    named_marks: [Option<Mark>; 62],
    /// The one end of the last Visual area that is set (`'<'`, its start, or `'>'`, its end) when `:mark <`
    /// or `:mark >` made it with no Visual area before (#1241). Vim's other end is then unset: both `<` and
    /// `>` read the one that is (so [`View::last_visual`] holds it twice), but `gv` has nothing to reselect.
    visual_one_end: Option<char>,
    /// Whether the Visual cursor was last moved with `curswant` at MAXCOL (`v$`, `$vj`), where nvim's
    /// `coladvance(MAXCOL)` leaves it on the NUL one past the line's last char, so the area's `'>` reads
    /// there (#1304). A Visual area begun after `$` without a move (`$v`) keeps the cursor on the last
    /// char. Reset on entering a selection.
    visual_eol: bool,
    /// Set by a dot-repeat that rebuilds a Visual area at the caret ([`prepare_selection_op`]): nvim's
    /// `redo_VIsual` runs the operator without making that area the last Visual one, so `'<`/`'>`, `gv`
    /// and `visualmode()` keep the area the user made (#1304). Consumed by the [`commit`] that leaves it.
    redo_visual: bool,
    /// Where Insert mode was last left (Vim's `` `^ `` mark), for `gi` to resume Insert there. Set in
    /// [`commit`] on an Insert→Normal transition to the pre-clamp caret, and snapped like the marks. `None`
    /// until the first Insert session ends.
    last_insert: Option<Mark>,
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
    /// Vim's `Insstart` column (char column) for `i_CTRL-G_j`/`k`: where the current insert run started —
    /// the caret at Insert entry, or where an edit next happened after a caret jump inside Insert
    /// (`insert_jumped`). Maintained in [`apply_command`].
    insert_col: usize,
    /// Vim's `Insstart_orig` as a byte offset, for `i_CTRL-U`: where the Insert session began (a caret key
    /// inside it does not move it), lowered when a delete reaches before it. Maintained with
    /// [`View::insert_col`].
    insstart: Option<usize>,
    /// The line number the Insert session began on and that line's width in cells then (nvim's
    /// `Insstart.lnum` and `Insstart_textlen`), for `'formatoptions'` `l` (#618).
    insstart_line: Option<(usize, usize)>,
    /// A caret jump (a cursor key, `CTRL-G j`, …) happened in Insert since the last edit, so the next edit
    /// restarts `insert_col` there.
    insert_jumped: bool,
    /// The jumplist (Vim): the cursor positions BEFORE each jump (search / `G` / `%` / a mark / paragraph),
    /// oldest → newest, bounded to [`MAX_CHANGES`]. Navigated by `CTRL-O` (older) / `CTRL-I` (newer) through
    /// [`jump_idx`]; recorded in [`apply_command`] and snapped every commit like the marks.
    jumps: Vec<usize>,
    /// `:keepjumps` (#609): while set, [`View::push_jump`] and [`View::push_change`] record nothing.
    pub(crate) keep_jumps: bool,
    /// The current cursor into [`jumps`] for `CTRL-O`/`CTRL-I`. Equals `jumps.len()` (past the newest) until
    /// the first `CTRL-O`, which saves the current position so `CTRL-I` can return, then walks the list.
    jump_idx: usize,
}

/// A stored Vim mark (#1304): `at`, the in-buffer byte offset a jump or an operator reads, and `past`, how
/// many bytes the mark's raw column lies beyond it. nvim keeps a mark as `(line, col)` and clamps the column
/// only where the mark is used (`check_cursor`), so an edit that shortens the mark's line leaves `getpos()`
/// reporting the old column, and a line that grows back finds the mark there again; a `$` area's `'>` is
/// one past its line's last char. `at` stays on the mark's line; the Document carries both through every
/// change ([`crate::LineMap::map_raw`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Mark {
    pub(crate) at: usize,
    pub(crate) past: usize,
}

impl Mark {
    /// A mark at `at`, its raw column its own.
    pub(crate) fn at(at: usize) -> Mark {
        Mark { at, past: 0 }
    }
}

/// The [`View::named_marks`] slot of mark `c`: `a`–`z` are 0–25, `A`–`Z` 26–51, `0`–`9` 52–61.
fn mark_slot(c: char) -> Option<usize> {
    match c {
        'a'..='z' => Some(c as usize - 'a' as usize),
        'A'..='Z' => Some(26 + c as usize - 'A' as usize),
        '0'..='9' => Some(52 + c as usize - '0' as usize),
        _ => None,
    }
}

/// Maximum entries kept in a View's change list (Vim's default `:changes` history is ~100).
const MAX_CHANGES: usize = 100;

/// The fixed-position entries of [`View::tracked`], before the change list and the jumplist.
const TRACKED_FIXED: usize = 67;

/// The editor over a single [`Document`] and its [`View`] — the top-level headless handle the TUI and
/// tests drive. F-007's Workspace will own many `(Document, View)` pairs referenced by handle; today
/// there is exactly one of each, composed here so the public API (`apply_command`, `cursor()`, …) is
/// unchanged while the buffer and view state are now cleanly separable.
pub struct EditorState {
    pub doc: Document,
    view: View,
    /// Who causes the edits this state makes (INV-ORIGIN): [`TransactionOrigin::UserInput`] unless the
    /// Workspace lends it under another origin, as for a macro replay ([`Workspace::set_origin`], #880).
    ///
    /// [`Workspace::set_origin`]: crate::Workspace::set_origin
    origin: TransactionOrigin,
    /// A command runs on the emptied buffer's line ([`EditorState::on_the_emptied_line`]): its own calls
    /// do not make the line again.
    on_emptied_line: bool,
}

mod view;

impl EditorState {
    /// A fresh editor over `initial` bytes, cursor at the start, Normal mode, marked saved (loaded content).
    pub fn new(initial: impl Into<Vec<u8>>) -> EditorState {
        let mut doc = Document::new(DocumentId(1), initial);
        doc.mark_saved();
        let view = View::fresh(doc.id());
        let mut st = EditorState {
            doc,
            view,
            origin: TransactionOrigin::UserInput,
            on_emptied_line: false,
        };
        // A caret on a leading `<Tab>` wants the tab's last cell (#1090).
        st.view.curswant = st.wanted_col();
        st
    }

    /// The invariant walker over the document and its view ([`Document::assert_invariants`],
    /// [`View::assert_invariants`]); panics on the first broken rule (#906).
    pub fn assert_invariants(&self) {
        assert_eq!(
            self.view.doc,
            self.doc.id(),
            "the view names another document"
        );
        self.doc.assert_invariants();
        self.view.assert_invariants(self.doc.bytes());
    }

    /// Apply `txn`, carrying this view's marks, change list and jumplist through it (#686). Every edit
    /// the editor makes goes through here or [`EditorState::with_tracked`].
    pub(crate) fn apply_txn(&mut self, txn: Transaction) -> Result<crate::pos::Revision, TxnError> {
        self.with_tracked(|doc, t| doc.apply_tracked(txn, t))
    }

    /// Run `f` on the document with this view's long-lived positions handed to it, then take them back.
    pub(crate) fn with_tracked<R>(
        &mut self,
        f: impl FnOnce(&mut Document, &mut [Tracked]) -> R,
    ) -> R {
        let mut t = self.view.tracked();
        // Vim's line model keeps the final newline; the Emacs profile's bytes do not (RFC-0024 §4). Only for
        // this command: a Document edited directly keeps its text as given.
        self.doc
            .set_keep_final_newline(self.view.caret == CaretGravity::OnChar);
        let r = f(&mut self.doc, &mut t);
        self.doc.set_keep_final_newline(false);
        self.view.untrack(&t);
        r
    }

    /// Apply `list` as an Ex command's own undo step (`BreakBefore`) carrying `origin`, the undo cursor at
    /// `cursor` when given. The Ex line commands and `:s` (in child modules) build their transaction here,
    /// in the transaction path CON-TXN-ONLY-MUTATION allows.
    pub(crate) fn apply_ex_edit(
        &mut self,
        list: EditList,
        origin: TransactionOrigin,
        cursor: Option<usize>,
    ) -> Result<crate::pos::Revision, TxnError> {
        let mut txn =
            Transaction::new(self.doc.revision(), list, origin).with_hint(GroupHint::BreakBefore);
        if let Some(c) = cursor {
            txn = txn.with_cursor(BytePos(c));
        }
        self.apply_txn(txn)
    }

    /// Make the edits that follow carry `origin` (INV-ORIGIN, #880).
    pub(crate) fn set_origin(&mut self, origin: TransactionOrigin) {
        self.origin = origin;
    }

    /// Whether [`EditorState::on_the_emptied_line`] makes the emptied buffer's line for a command now.
    fn makes_the_emptied_line(&self) -> bool {
        !self.on_emptied_line
            && self.doc.is_empty()
            && self.view.caret == CaretGravity::OnChar
            && self.doc.is_modifiable()
    }

    /// Run `f`, a command that adds text, on the emptied buffer's one line (RFC-0024 §5). Vim's `ML_EMPTY`
    /// buffer shows one empty line, and what a command adds goes into it or around it: `o` there gives two
    /// empty lines, `:put` puts after it, `P` and `:0r` before it, and typed text (`a`, `<CR>`) goes before
    /// its newline. The text is empty, so that line is made first, as `\n`, in the same undo step as `f`'s
    /// change; when `f` changes nothing, the line is taken back, so the buffer stays emptied and is not
    /// modified. `f` just runs on a buffer that is not empty, in the Emacs profile (whose empty buffer is no
    /// text), on a `'nomodifiable'` buffer, and inside another call of this.
    pub(crate) fn on_the_emptied_line<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        if !self.makes_the_emptied_line() {
            return f(self);
        }
        self.on_emptied_line = true;
        let block = !self.doc.in_undo_block();
        if block {
            self.doc.begin_undo_block();
        }
        let line = EditList::new(vec![Edit::insert(BytePos(0), b"\n".to_vec())]).map(|list| {
            Transaction::new(self.doc.revision(), list, self.origin)
                .with_hint(GroupHint::BreakBefore)
        });
        // The line is the emptied buffer's own: the Document adds no newline to it.
        let made = line.is_ok_and(|txn| {
            self.with_tracked(|doc, t| {
                doc.set_keep_final_newline(false);
                doc.apply_tracked(txn, t)
            })
            .is_ok()
        });
        let after_line = self.doc.revision();
        let r = f(self);
        let unchanged = made && self.doc.revision() == after_line;
        if unchanged {
            // Take the line back as though it was never made: no undo step, no redo branch lost, and the
            // buffer is not modified; the marks the command set on it go back to the start with it.
            self.with_tracked(|doc, t| doc.retract_tracked(t));
            self.view.cursor = 0;
            // What the command stored on the line (the Insert start of `gi`, …) comes back into the text.
            self.settle_offsets();
        }
        if block {
            self.doc.end_undo_block();
        }
        self.on_emptied_line = false;
        r
    }

    /// Reconstitute an editing context from a Document and a View taken out of a Workspace arena — the
    /// inverse of [`EditorState::into_parts`]. The [`crate::workspace::Workspace`] swaps the focused
    /// `(Document, View)` into an `EditorState` to run the UNCHANGED `plan`/`commit` pipeline, then
    /// swaps them back; this keeps the single-window path byte-identical (F-007 step (b)).
    #[must_use]
    pub(crate) fn from_parts(doc: Document, view: View) -> EditorState {
        EditorState {
            doc,
            view,
            origin: TransactionOrigin::UserInput,
            on_emptied_line: false,
        }
    }

    /// Split an editing context back into its owned Document and View, to return them to the arena.
    #[must_use]
    pub(crate) fn into_parts(self) -> (Document, View) {
        (self.doc, self.view)
    }

    /// Exchange this context's register store with `store`. The [`crate::workspace::Workspace`] owns the
    /// one editor-global store (D-057 slice 2, #944) and lends it into the focused view for one command, so a
    /// View at rest in the arena holds an empty store and a yank reaches every buffer and split.
    pub(crate) fn swap_registers(&mut self, store: &mut RegisterStore) {
        std::mem::swap(&mut self.view.registers, store);
    }

    /// After an edit applied outside [`commit`]: snap the cursor and every stored offset into the new
    /// buffer (#691). Ex edits bypass `commit`, so without this they leave offsets mid-character.
    fn settle_offsets(&mut self) {
        let bytes = self.doc.bytes();
        self.view.cursor = snap(bytes, self.view.cursor);
        self.view.snap_offsets(bytes);
    }
}

mod options;
pub use options::{
    check_comments, check_formatoptions, CaretGravity, EditorOption, IndentStyle, MAX_OPTION_INT,
    MAX_TABSTOP,
};
pub(crate) use options::{stops_text, FormatConfig, ViewOptions};
use options::{IndentConfig, SearchCase};

mod want;
pub(crate) use want::Want;
use want::WantKind;

mod action;
pub use action::Plan;
use action::{Action, BlockGeom, BlockInsert, MarkWrite, RegWrite};

mod visual_repeat;
use visual_repeat::{prepare_selection_op, VisualOpGeom};

mod accessors;

mod sort;
pub use sort::{SortKind, SortOptions};

mod range;
pub(crate) use range::*;

mod search;
use search::{
    landing_key, match_spans, search_bwd, search_fwd, search_spans, step_char, walk_count,
};

mod line_cmds;
mod substitute;

pub(crate) use substitute::{
    find_substitutions, line_count, line_span, line_spans, resolve_line_range, resolve_lines,
};

pub use substitute::{GlobalCmd, LineAddr, SubFlags, SubOutcome, SubRange, Substitution};

mod ex_cmd;
#[cfg(test)]
pub(crate) use ex_cmd::samples as ex_cmd_samples;
pub use ex_cmd::{
    ExCmd, ExOutcome, GroupKind, MAX_APPLY_EDITS, MAX_APPLY_EDIT_BYTES, MAX_SHIFT_LEVELS,
};

mod format;
pub use format::OptionValueError;
mod indent;
mod lisp;
pub use lisp::DEFAULT_LISPWORDS;
mod cindent;
pub(crate) use cindent::check_linecomment as c_line_comment;
pub use cindent::{CinKey, CinOptions, DEFAULT_CINKEYS, DEFAULT_CINSCOPEDECLS};
pub(crate) use lisp::line_comment as lisp_line_comment;
mod planner;
pub use planner::plan;
pub(crate) use planner::word_search;

/// The `*_tracked` undo-tree steps the commit pipeline takes. Kept in this file, with
/// [`EditorState::apply_planned_txn`] and [`EditorState::line_undo_at`], because only here may code build a
/// transaction or call the tracked undo family (CON-TXN-ONLY-MUTATION, `spec/architecture.yaml`).
#[derive(Clone, Copy)]
enum UndoStep {
    /// `u`.
    Undo,
    /// `CTRL-R`.
    Redo,
    /// `g-` (`older`) / `g+`.
    Chrono { older: bool },
}

impl UndoStep {
    fn take(
        self,
        d: &mut Document,
        cursor: usize,
        t: &mut [Tracked],
    ) -> Option<crate::undo_cursor::UndoCursor> {
        match self {
            UndoStep::Undo => d.undo_tracked(BytePos(cursor), t),
            UndoStep::Redo => d.redo_tracked(BytePos(cursor), t),
            UndoStep::Chrono { older } => d.chronological_tracked(older, BytePos(cursor), t),
        }
    }
}

impl EditorState {
    /// Apply a plan's edits as one transaction whose cursor is `start`, where the change began.
    fn apply_planned_txn(&mut self, edits: EditList, hint: GroupHint, start: usize) {
        let txn = Transaction::new(self.doc.revision(), edits, self.origin)
            .with_hint(hint)
            .with_cursor(BytePos(start));
        // The plan built the edits from the current buffer, so apply cannot be stale or out of range.
        self.apply_txn(txn)
            .expect("planned transaction applies cleanly");
    }

    /// `U`: undo the latest changes on one line; where it leaves the cursor.
    fn line_undo_at(&mut self, cursor: usize) -> Option<usize> {
        let origin = self.origin;
        self.with_tracked(|d, t| d.line_undo_tracked(BytePos(cursor), origin, t))
            .map(|p| p.0)
    }
}

mod commit;
#[cfg(test)]
use commit::through_change;
pub use commit::{apply_command, commit};

#[cfg(test)]
mod tests;
