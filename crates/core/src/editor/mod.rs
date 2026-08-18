//! The editor state and the pure **plan / commit** command pipeline (RFC-0012).
//!
//! [`plan`] is a *pure decision*: `(&EditorState, &Command) -> Plan`, no mutation, no IO. [`commit`] applies
//! a `Plan` and returns the [`Effect`]s the frontend must perform. Because the core never does IO, replaying
//! the same commands on the same initial document is deterministic (see [`crate::trace`]). This is the split
//! that captures most of a Haskell rewrite's benefit in Rust — enforced by an empty dependency set.

use crate::command::{
    BlockInsertKind, Command, ForcedWise, OpKind, SearchOp, SelectKind, WordCase,
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
            Mode::Normal | Mode::Insert | Mode::Replace | Mode::VirtualReplace => None,
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
    /// The indent config (`editor.tab_width` + `editor.indent_style`).
    indent: IndentConfig,
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
}

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
            registers: RegisterStore::new(),
            pending_register: None,
            anchor: None,
            mark: None,
            last_visual: None,
            replace_stack: Vec::new(),
            block_insert: None,
            indent: IndentConfig {
                tab_width: 4,
                style: IndentStyle::Space,
            },
            curswant: 0,
            search_case: SearchCase {
                ignore: false,
                smart: false,
            },
            caret: CaretGravity::OnChar,
        }
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

/// `curswant` sentinel meaning "the end of whatever line you land on" — Vim's MAXCOL, set by `$`.
const MAXCOL: usize = usize::MAX;

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
}

/// How a committed command should route its captured text into the register store. A `Yank` additionally
/// seeds the yank register `"0` (when unregistered); an `Edit` (delete/change/`x`) never touches `"0`, so
/// `"0` survives intervening deletes (Vim `:help quote0`).
enum RegWrite {
    Edit(Register),
    Yank(Register),
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

/// Whether a motion is a text object (a range around the cursor), as opposed to a bare cursor movement. In a
/// selection mode these set both ends of the selection; everywhere else they are operator operands.
fn is_text_object(m: Motion) -> bool {
    matches!(
        m,
        Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::Pair { .. }
            | Motion::Quote { .. }
    )
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
    pub fn global(
        &mut self,
        range: SubRange,
        pattern: &str,
        negate: bool,
        cmd: &GlobalCmd,
    ) -> Result<usize, RegexError> {
        let opts = self.view.search_options();
        let re = crate::pattern::Regex::compile(pattern, opts)?;
        let bytes = self.doc.bytes();
        let hay = std::str::from_utf8(bytes)
            .map_err(|_| RegexError::Syntax("buffer is not valid UTF-8".into()))?;
        let lines = line_spans(hay);
        // `:g` with no range is the WHOLE FILE (unlike `:s`, whose default is the current line).
        let (first, last) = match range {
            SubRange::CurrentLine | SubRange::WholeFile => (0, lines.len().saturating_sub(1)),
            SubRange::Lines(a, b) => (
                a.saturating_sub(1).min(lines.len().saturating_sub(1)),
                b.saturating_sub(1).min(lines.len().saturating_sub(1)),
            ),
        };
        // PASS 1: mark the matching (or non-matching) lines against the untouched buffer.
        let marked: Vec<usize> = (first..=last)
            .filter(|&li| {
                let (ls, le) = lines[li];
                re.find_at(&hay[ls..le], 0).is_some() != negate
            })
            .collect();
        if marked.is_empty() {
            return Ok(0);
        }

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
        self.view.cursor = pos;
        let b = self.doc.bytes();
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

/// The byte range `[s, e)` a `delete`/`yank` operator covers for a motion + count, plus whether the removed
/// span is **linewise** (its register geometry and paste shape). Linewise motions delete whole lines;
/// paragraph motions (`d}`) become linewise per Vim's exclusive-linewise rule; everything else is charwise.
fn op_span(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    // Whole-lines span from the cursor's line through the target's line, inclusive of the final newline.
    let whole_lines = |t: usize| {
        let start = line_start(b, cur.min(t));
        let le = line_end(b, cur.max(t));
        let end = if le < b.len() { le + 1 } else { le };
        (start, end, true)
    };
    match m {
        // Line jumps (`dG`, `dgg`, `d{n}G`) are linewise across every line between the cursor and target.
        Motion::GotoLine | Motion::LastLine => whole_lines(motion::target(b, cur, m, count)),
        // Vertical motions under an operator are linewise (`dj` deletes this line and the next). A motion
        // that cannot move a line (`dj` on the last line) fails the operator entirely (Vim) — a no-op range.
        Motion::Up | Motion::Down => {
            let t = motion::target(b, cur, m, count);
            if line_start(b, t) == line_start(b, cur) {
                (cur, cur, true)
            } else {
                whole_lines(t)
            }
        }
        // `dd` / `{count}dd`: whole lines from the cursor's line down.
        Motion::Line => {
            let start = line_start(b, cur);
            let mut end = start;
            for _ in 0..count.max(1) {
                let le = line_end(b, end);
                if le < b.len() {
                    end = le + 1;
                } else {
                    end = le;
                    break;
                }
            }
            (start, end, true)
        }
        // Paragraph objects (`dip`/`dap`) are linewise (Vim); `char_span` already returns whole lines.
        Motion::InnerParagraph | Motion::AParagraph => {
            let (s, e) = motion::char_span(b, cur, m, count);
            (s, e, true)
        }
        // Paragraph motions (`d}`/`d{`) are exclusive charwise, but Vim's exclusive-linewise rule can turn
        // them linewise — shared with forced-charwise on a linewise motion (see `exclusive_linewise`).
        Motion::ParagraphFwd | Motion::ParagraphBack => {
            let t = motion::target(b, cur, m, count);
            exclusive_linewise(b, cur.min(t), cur.max(t))
        }
        // Everything else is the motion's charwise span.
        _ => {
            let (s, e) = motion::char_span(b, cur, m, count);
            (s, e, false)
        }
    }
}

/// Vim's exclusive-linewise reduction for an EXCLUSIVE charwise span `[lo, hi)`: if the end sits at column
/// 0 of a line and the start is at/before that start line's first non-blank, the span becomes whole lines
/// (linewise); otherwise the end pulls back one byte (charwise). Empty span → a no-op. Shared by the `d}`
/// paragraph motion and forced-charwise on a linewise motion (`dvj`), which is why they agree.
fn exclusive_linewise(b: &[u8], lo: usize, hi: usize) -> (usize, usize, bool) {
    if lo >= hi {
        (lo, lo, false)
    } else if hi > 0 && hi == line_start(b, hi) {
        if lo <= motion::first_non_blank(b, lo) {
            (line_start(b, lo), hi, true)
        } else {
            (lo, hi - 1, false)
        }
    } else {
        (lo, hi, false)
    }
}

/// Whether a charwise motion is INCLUSIVE (its last char is part of the operated span) in Vim terms — the
/// bit `o_v` toggles. Only the directional motions matter here; text objects carry their own exact span and
/// are left untoggled by [`forced_span`]. Kept in sync with `motion::char_span`'s inclusivity by hand.
fn motion_inclusive(m: Motion) -> bool {
    matches!(
        m,
        Motion::WordEnd
            | Motion::BigWordEnd
            | Motion::LineEnd
            | Motion::MatchBracket
            | Motion::FindChar { forward: true, .. }
    )
}

/// The span for an operator whose motion wise is FORCED (Vim `o_v`/`o_V`). `Linewise` expands the motion's
/// reach to whole lines; `Charwise` makes it charwise — turning a linewise motion into an inclusive span to
/// the target, or toggling a charwise motion's exclusive/inclusive edge. Text objects (which already carry
/// an exact span) are only reshaped for `Linewise`; `Charwise` leaves their edge alone.
fn forced_span(
    b: &[u8],
    cur: usize,
    m: Motion,
    count: u32,
    wise: ForcedWise,
) -> (usize, usize, bool) {
    let (s0, e0, was_line) = op_span(b, cur, m, count);
    match wise {
        ForcedWise::Linewise => {
            // Whole lines from the cursor's line THROUGH the line the motion lands on (Vim `dV}` includes
            // the blank paragraph line the `}` reaches). Text objects have no bare-move target, so expand
            // their own span instead.
            if is_text_object(m) || motion::target(b, cur, m, count) == cur && !was_line {
                if s0 >= e0 {
                    return (cur, cur, true);
                }
                let start = line_start(b, s0);
                let le = line_end(b, (e0 - 1).max(s0));
                return (start, if le < b.len() { le + 1 } else { le }, true);
            }
            let t = motion::target(b, cur, m, count);
            let (lo, hi) = (cur.min(t), cur.max(t));
            let start = line_start(b, lo);
            let le = line_end(b, hi);
            (start, if le < b.len() { le + 1 } else { le }, true)
        }
        ForcedWise::Charwise => {
            if was_line {
                // A linewise motion forced charwise becomes EXCLUSIVE [cur, target) and then takes Vim's
                // exclusive-linewise reduction — so `dvj` deletes the first line linewise, exactly as `d}`
                // would (both go through `exclusive_linewise`).
                let t = motion::target(b, cur, m, count);
                exclusive_linewise(b, cur.min(t), cur.max(t))
            } else if is_text_object(m) {
                (s0, e0, false)
            } else if motion_inclusive(m) {
                (s0, prev_boundary(b, e0).max(s0), false)
            } else {
                (s0, next_boundary(b, e0).min(b.len()), false)
            }
        }
        // Blockwise force never reaches here — the `OpForced` arm routes it to `block_op` (a rectangle is
        // not a single span, which is all `forced_span` can return).
        ForcedWise::Blockwise => {
            unreachable!("forced blockwise is handled by block_op before forced_span")
        }
    }
}

/// The byte range a `change` operator covers: for `Motion::Line` it is the *content* of the line(s) (the
/// newline is kept so `cc` leaves an empty line to type into); else the same charwise span as delete.
fn change_range(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize) {
    // Line jumps under change keep the final newline, leaving an empty line to type into (as `cc` does).
    if matches!(m, Motion::GotoLine | Motion::LastLine) {
        let t = motion::target(b, cur, m, count);
        let start = line_start(b, cur.min(t));
        return (start, line_end(b, cur.max(t)));
    }
    if m != Motion::Line {
        return motion::char_span(b, cur, m, count);
    }
    let start = line_start(b, cur);
    let mut pos = cur;
    for _ in 1..count.max(1) {
        let le = line_end(b, pos);
        if le < b.len() {
            pos = le + 1;
        } else {
            break;
        }
    }
    (start, line_end(b, pos))
}

/// The byte range `[s, e)` a Visual selection covers, from its anchor and active (cursor) ends. Charwise
/// includes the character under the higher end (Vim's inclusive selection); linewise spans whole lines
/// including the trailing newline where present.
fn selection_range(b: &[u8], anchor: usize, cursor: usize, line: bool) -> (usize, usize) {
    let lo = anchor.min(cursor);
    let hi = anchor.max(cursor);
    if line {
        let start = line_start(b, lo);
        let le = line_end(b, hi);
        let end = if le < b.len() { le + 1 } else { le };
        (start, end)
    } else {
        // Inclusive of the char under `hi`.
        let end = if hi < b.len() {
            next_boundary(b, hi)
        } else {
            hi
        };
        (lo, end)
    }
}

/// The geometry of a BLOCKWISE selection whose two corners are the byte offsets `anchor` and `cursor`.
/// Returns one `[start, end)` byte range per line the rectangle crosses (top line first), plus the block's
/// inclusive char-column bounds `(col_lo, col_hi)`.
///
/// Columns are CHAR columns (via [`col_of`]/[`at_col`]) — correct for ASCII and multibyte code points;
/// tab/wide-char VISUAL columns are a documented follow-up (the same curswant family as the i_CTRL-O gap).
/// Each row's range is clamped to that line's own length, so a line shorter than `col_lo` yields an empty
/// range at its end (Vim: short lines contribute nothing to a block delete/yank).
fn block_rows(b: &[u8], anchor: usize, cursor: usize) -> (Vec<(usize, usize)>, usize, usize) {
    let a_ls = line_start(b, anchor);
    let c_ls = line_start(b, cursor);
    let a_col = col_of(b, a_ls, anchor);
    let c_col = col_of(b, c_ls, cursor);
    let col_lo = a_col.min(c_col);
    let col_hi = a_col.max(c_col);
    let top_ls = a_ls.min(c_ls);
    let bot_ls = a_ls.max(c_ls);

    let mut rows = Vec::new();
    let mut rs = top_ls;
    loop {
        // `at_col` clamps to the line end, so on a short line `s == e == line_end` (an empty slice).
        let s = at_col(b, rs, col_lo);
        let e = at_col(b, rs, col_hi + 1);
        rows.push((s, e));
        let le = line_end(b, rs);
        if rs >= bot_ls || le >= b.len() {
            break;
        }
        rs = le + 1;
    }
    (rows, col_lo, col_hi)
}

/// Walk `count` char boundaries forward from `from`, never past `limit` (typically the line end).
/// Returns the end byte offset; fewer than `count` chars available stops at `limit` (Vim's EOL clamp).
fn advance_n(b: &[u8], from: usize, count: u32, limit: usize) -> usize {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            break;
        }
        let nb = next_boundary(b, end).min(limit);
        if nb == end {
            break;
        }
        end = nb;
    }
    end
}

/// Like [`advance_n`] but reports whether the FULL `count` chars fit within `[from, limit)`. The bool
/// is false when fewer than `count` chars remain — the signal `{count}r` uses to become a clean no-op.
fn advance_n_checked(b: &[u8], from: usize, count: u32, limit: usize) -> (usize, bool) {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            return (end, false);
        }
        end = next_boundary(b, end);
    }
    (end, true)
}

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

/// One pending substitution from [`EditorState::substitute_preview`]: the byte span `[start, end)` to
/// replace and the (expanded) replacement bytes, plus the 0-based line it sits on. Absolute offsets into
/// the CURRENT buffer — valid only until the buffer is edited, so an interactive confirm loop collects
/// the ACCEPTED subset and applies it all at once ([`EditorState::apply_substitutions`]).
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
fn line_spans(hay: &str) -> Vec<(usize, usize)> {
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
fn expand_replacement(replacement: &str, matched: &str) -> Vec<u8> {
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

/// The byte offset of the next Vim-regex match at/after `from`, wrapping to the document start (F-009).
/// Invalid UTF-8, an unrepresentable/malformed pattern, or no match yields `None` — the caller keeps the
/// cursor (Vim rings the bell). Replaces the v0 literal `search::find_next`.
fn search_fwd(
    b: &[u8],
    pattern: &str,
    from: usize,
    opts: crate::pattern::Options,
) -> Option<usize> {
    let hay = std::str::from_utf8(b).ok()?;
    let mut from = from.min(hay.len());
    while from < hay.len() && !hay.is_char_boundary(from) {
        from += 1; // a regex search must start on a char boundary
    }
    let re = crate::pattern::Regex::compile(pattern, opts).ok()?;
    re.find_at(hay, from)
        .or_else(|| re.find_at(hay, 0))
        .map(|m| m.start)
}

/// The byte offset of the previous match starting strictly before `before`, wrapping to the last match.
fn search_bwd(
    b: &[u8],
    pattern: &str,
    before: usize,
    opts: crate::pattern::Options,
) -> Option<usize> {
    let hay = std::str::from_utf8(b).ok()?;
    let re = crate::pattern::Regex::compile(pattern, opts).ok()?;
    let all = re.find_all(hay);
    all.iter()
        .rev()
        .find(|m| m.start < before)
        .or_else(|| all.last())
        .map(|m| m.start)
}

mod planner;
pub use planner::plan;

/// Apply a plan to the state, returning the effects the frontend must perform.
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
    match plan.action {
        Action::Txn { edits, hint } => {
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
        Action::BlockInsertArm {
            edits,
            hint,
            session,
        } => {
            if !edits.is_empty() {
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
    st.view.cursor = snap(st.doc.bytes(), plan.cursor);
    st.view.mode = plan.mode;
    st.view.last_was_edit = plan.is_edit;
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
    // mirroring the unnamed slot on a named write; append (`"A`) is handled inside the store.
    match plan.set_register {
        Some(RegWrite::Edit(reg)) => st.view.registers.write(st.view.pending_register, reg),
        Some(RegWrite::Yank(reg)) => st.view.registers.yank(st.view.pending_register, reg),
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

/// Convenience: plan then commit one command, then maintain the sticky desired column (curswant).
pub fn apply_command(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    let p = plan(st, cmd);
    let effects = commit(st, p);
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
        | Command::EnterVirtualReplace => {
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
