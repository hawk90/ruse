//! The editor state and the pure **plan / commit** command pipeline (RFC-0012).
//!
//! [`plan`] is a *pure decision*: `(&EditorState, &Command) -> Plan`, no mutation, no IO. [`commit`] applies
//! a `Plan` and returns the [`Effect`]s the frontend must perform. Because the core never does IO, replaying
//! the same commands on the same initial document is deterministic (see [`crate::trace`]). This is the split
//! that captures most of a Haskell rewrite's benefit in Rust — enforced by an empty dependency set.

use std::borrow::Cow;

use crate::anchor::Tracked;
use crate::cells::{Tabs, VarStops};
use crate::command::{
    BlockInsertKind, Command, ForcedWise, MarkOp, OpKind, OpenKind, SearchOffset, SearchOp,
    SelectKind, VisualOp, WordCase,
};
use crate::document::{Document, DocumentId, TxnError};
use crate::edit::{Edit, EditList};
use crate::effect::Effect;
use crate::motion::{
    self, at_col, col_of, line_end, line_start, next_boundary, next_grapheme, prev_boundary,
    prev_grapheme, snap, Motion,
};
use crate::pattern::RegexError;
use crate::pos::BytePos;
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
/// carry their on/off value (`:set ic` = `IgnoreCase(true)`, `:set noic` = `IgnoreCase(false)`). The string
/// options carry their text, so the type is `Clone`, not `Copy`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditorOption {
    /// `'ignorecase'` (`ic`).
    IgnoreCase(bool),
    /// `'smartcase'` (`scs`).
    SmartCase(bool),
    /// `'shiftwidth'` (`sw`) — one indent level's width (`editor.shift_width`). `0` follows `'tabstop'`
    /// (Vim's `sw=0`).
    ShiftWidth(usize),
    /// `'tabstop'` (`ts`) — the columns a `<Tab>` character spans (`editor.tab_width`): rendering, virtual
    /// columns, `<Tab>` insertion stops and the tabs+spaces mix of a `noexpandtab` indent. Clamped to
    /// `1..=`[`MAX_TABSTOP`].
    TabStop(usize),
    /// `'expandtab'` (`et`) — indent with spaces (`true`) vs a tab (`false`).
    ExpandTab(bool),
    /// `'textwidth'` (`tw`) — the width `gq`/`gw` wrap to; `0` means use the 79-column fallback.
    TextWidth(usize),
    /// `'softtabstop'` (`sts`) — the columns a `<Tab>` / `<BS>` in Insert counts as while editing; `0` is
    /// off, a negative value follows the effective `'shiftwidth'` (#617).
    SoftTabStop(i64),
    /// `'autoindent'` (`ai`) — a line opened with `o`/`O`/`<CR>` takes the indent of the line it opens from,
    /// and `cc`/`S` keep the line's indent (#617).
    AutoIndent(bool),
    /// `'smartindent'` (`si`) — C-like extra indent after `{` or a `'cinwords'` keyword, and `{`/`}`/`#`
    /// realigned when typed first on a new line (#617).
    SmartIndent(bool),
    /// `'copyindent'` (`ci`) — an autoindent copies the source line's indent characters as far as they fit
    /// (#617).
    CopyIndent(bool),
    /// `'preserveindent'` (`pi`) — changing a line's indent (`>>`, `<<`, `i_CTRL-T`, `i_CTRL-D`) keeps as
    /// much of its existing indent characters as fit (#617).
    PreserveIndent(bool),
    /// `'smarttab'` (`sta`, Vim-global) — `<Tab>`/`<BS>` in a line's indent step by `'shiftwidth'` (#617).
    SmartTab(bool),
    /// `'shiftround'` (`sr`, Vim-global) — `>`/`<` round the indent to a multiple of `'shiftwidth'` (#617).
    ShiftRound(bool),
    /// The caret gravity the input profile installs (D-050; not a Vim option, so `:set` never makes it).
    /// The core reads it on every edit, so a trace starts with it (RFC-0020 R4, #1134): an Emacs session
    /// replays with point between characters.
    CaretGravity(CaretGravity),
    /// `'formatoptions'` (`fo`) — the flag letters that steer auto-wrap, comment leaders, `gq` and `J`
    /// (#618). nvim's default is `tcqj`.
    FormatOptions(String),
    /// `'comments'` (`com`) — the comma-separated `{flags}:{string}` comment leaders (#618).
    Comments(String),
    /// `'commentstring'` (`cms`) — the `%s` template the `gc` comment operator wraps a line in (#618).
    CommentString(String),
    /// `'formatlistpat'` (`flp`) — the pattern of a numbered-list item for `'formatoptions'` `n` (#618).
    FormatListPat(String),
    /// `'formatprg'` (`fp`, global here) — the external program `gq` filters lines through (#618).
    FormatPrg(String),
    /// `'equalprg'` (`ep`, global here) — the external program `=` filters lines through (#617).
    EqualPrg(String),
    /// `'cinwords'` (`cinw`) — the comma-separated keywords after which `'smartindent'` indents the next
    /// line (#617).
    CinWords(String),
    /// `'joinspaces'` (`js`, Vim-global) — `J` and `gq` put two spaces after a `.`, `?` or `!` (#618).
    JoinSpaces(bool),
    /// `'wrapmargin'` (`wm`) — with `'textwidth'` 0, wrap this many columns before the window's right edge
    /// (#618).
    WrapMargin(i64),
    /// `'vartabstop'` (`vts`) — the comma-separated widths of the successive tab stops, the last one
    /// repeating; empty is off, and then `'tabstop'` applies (#1163). An invalid list is ignored.
    VarTabStop(String),
    /// `'varsofttabstop'` (`vsts`) — the same for the soft tab stops an Insert `<Tab>`/`<BS>` steps by;
    /// empty is off, and then `'softtabstop'` applies (#1163).
    VarSoftTabStop(String),
    /// `'lisp'` — `=` and, under `'autoindent'`, `o`/`O`/`<CR>`/`cc` indent by the Lisp rules (#1163).
    Lisp(bool),
    /// `'lispwords'` (`lw`) — the comma-separated words whose forms `'lisp'` indents as a body (#1163).
    LispWords(String),
    /// `'cindent'` (`cin`) — `o`/`O`/`<CR>`, `cc`/`S` and the keys of `'cinkeys'` indent by the C rules
    /// (`=` always does, as in nvim when no `'indentexpr'` is set, #1163).
    CIndent(bool),
    /// `'cinoptions'` (`cino`) — how the C indent lays out code (#1163).
    CinOptions(String),
    /// `'cinkeys'` (`cink`) — the keys that re-indent the line in Insert mode under `'cindent'` (#1163).
    CinKeys(String),
    /// `'cinscopedecls'` (`cinsd`) — the C++ scope declarations the C indent knows (#1163).
    CinScopeDecls(String),
}

impl EditorOption {
    /// Whether the option is GLOBAL in Vim (`'ignorecase'`, `'smartcase'`): one value for every window,
    /// so `:set`, `:setlocal` and `:setglobal` all write it everywhere. The others are buffer-local, and
    /// ruse keeps their local value per View with the global copy on the Workspace.
    #[must_use]
    pub fn is_global(&self) -> bool {
        // The caret gravity is the input profile's, one for the whole session (`Workspace::set_caret_gravity`).
        matches!(
            self,
            EditorOption::IgnoreCase(_)
                | EditorOption::SmartCase(_)
                | EditorOption::SmartTab(_)
                | EditorOption::ShiftRound(_)
                | EditorOption::CaretGravity(_)
                | EditorOption::JoinSpaces(_)
                | EditorOption::FormatPrg(_)
                | EditorOption::EqualPrg(_)
        )
    }
}

/// Whether `value` is a valid `'formatoptions'` (nvim's `FO_ALL`), else nvim's error message (#618).
///
/// # Errors
/// `E539: Illegal character <c>` for a letter no flag uses.
pub fn check_formatoptions(value: &str) -> Result<(), OptionValueError> {
    format::check_formatoptions(value)
}

/// Whether `value` is a valid `'comments'` (nvim's `did_set_comments`), else nvim's error message (#618).
///
/// # Errors
/// `E539` for a bad flag, `E524` for a part without `:`, `E525` for an empty leader.
pub fn check_comments(value: &str) -> Result<(), OptionValueError> {
    format::check_comments(value)
}

/// The largest value a number option holds (nvim's C `int`).
pub const MAX_OPTION_INT: usize = i32::MAX as usize;

/// The largest `'tabstop'` (nvim's `TABSTOP_MAX`).
pub const MAX_TABSTOP: usize = 9999;

/// The indent config a shift/indent operator (`>>`/`<<`) reads: `editor.tab_width` (`'tabstop'`),
/// `editor.shift_width` (`'shiftwidth'`) and `editor.indent_style` (`'expandtab'`). Grouped so the "config"
/// concern is one field on [`View`], not four loose ones.
#[derive(Clone, Copy)]
struct IndentConfig {
    /// `editor.tab_width` / `'tabstop'` — the columns one `\t` spans. Schema default 4. Always `>= 1`.
    tab_width: usize,
    /// `editor.shift_width` / `'shiftwidth'` — one indent level's width in columns. `0` follows
    /// [`IndentConfig::tab_width`], as Vim's `sw=0` does. Schema default 4.
    shift_width: usize,
    /// `editor.indent_style` — whether an indent level is spaces or a tab. Schema default `space`.
    style: IndentStyle,
    /// `'softtabstop'`: `0` off, negative follows the effective shiftwidth (`editor.softtabstop`).
    soft_tab: i64,
    /// `'vartabstop'` (`vts`): the widths of the successive tab stops; wins over `tab_width` when set
    /// (#1163).
    vts: VarStops,
    /// `'varsofttabstop'` (`vsts`): the widths of the successive soft tab stops; wins over `soft_tab`
    /// when set (#1163).
    vsts: VarStops,
    /// `'autoindent'` (`editor.autoindent`, default on as in nvim).
    autoindent: bool,
    /// `'smartindent'` (`editor.smartindent`).
    smartindent: bool,
    /// `'copyindent'` (`editor.copyindent`).
    copyindent: bool,
    /// `'preserveindent'` (`editor.preserveindent`).
    preserveindent: bool,
    /// `'smarttab'` (`editor.smarttab`, default on as in nvim). Vim-global.
    smarttab: bool,
    /// `'shiftround'` (`editor.shiftround`). Vim-global.
    shiftround: bool,
}

/// The search-case config: `'ignorecase'` and `'smartcase'` (F-009). Default off — the factory Vim
/// default the differential oracle runs against.
#[derive(Clone, Copy)]
struct SearchCase {
    ignore: bool,
    smart: bool,
}

/// The text-formatting options (#618): `'formatoptions'`, `'comments'`, `'commentstring'`,
/// `'formatlistpat'`, `'formatprg'`, `'joinspaces'` and `'wrapmargin'`. The strings borrow nvim's defaults
/// until a `:set` replaces them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct FormatConfig {
    pub(crate) options: Cow<'static, str>,
    pub(crate) comments: Cow<'static, str>,
    pub(crate) commentstring: Cow<'static, str>,
    pub(crate) listpat: Cow<'static, str>,
    pub(crate) prg: Cow<'static, str>,
    pub(crate) equalprg: Cow<'static, str>,
    pub(crate) cinwords: Cow<'static, str>,
    pub(crate) lisp: bool,
    pub(crate) lispwords: Cow<'static, str>,
    pub(crate) cindent: bool,
    pub(crate) cinoptions: Cow<'static, str>,
    pub(crate) cinkeys: Cow<'static, str>,
    pub(crate) cinscopedecls: Cow<'static, str>,
    pub(crate) joinspaces: bool,
    pub(crate) wrapmargin: i64,
}

impl FormatConfig {
    /// Whether `'formatoptions'` holds flag `c`.
    pub(crate) fn has(&self, c: char) -> bool {
        self.options.contains(c)
    }
}

/// The option values a [`View`] carries (indent, `'textwidth'`, formatting, search case, caret gravity),
/// grouped so the Workspace can keep the GLOBAL values Vim's `:set` also writes and hand them to every View
/// it creates (`:enew`, `:e`, `:bd`'s replacement), as Vim copies global options into a new buffer (#754).
#[derive(Clone)]
pub(crate) struct ViewOptions {
    indent: IndentConfig,
    text_width: usize,
    format: FormatConfig,
    search_case: SearchCase,
    caret: CaretGravity,
}

impl ViewOptions {
    /// Vim's factory values (`spec/config-schema.yaml` for the indent: `editor.tab_width=4`,
    /// `editor.indent_style=space`), on-character caret.
    pub(crate) const DEFAULT: ViewOptions = ViewOptions {
        indent: IndentConfig {
            tab_width: 4,
            shift_width: 4,
            style: IndentStyle::Space,
            soft_tab: 0,
            vts: VarStops::NONE,
            vsts: VarStops::NONE,
            autoindent: true,
            smartindent: false,
            copyindent: false,
            preserveindent: false,
            smarttab: true,
            shiftround: false,
        },
        text_width: 0,
        format: FormatConfig {
            options: Cow::Borrowed("tcqj"),
            comments: Cow::Borrowed(format::DEFAULT_COMMENTS),
            commentstring: Cow::Borrowed(""),
            listpat: Cow::Borrowed(format::DEFAULT_LISTPAT),
            prg: Cow::Borrowed(""),
            equalprg: Cow::Borrowed(""),
            cinwords: Cow::Borrowed(indent::DEFAULT_CINWORDS),
            lisp: false,
            lispwords: Cow::Borrowed(lisp::DEFAULT_LISPWORDS),
            cindent: false,
            cinoptions: Cow::Borrowed(""),
            cinkeys: Cow::Borrowed(cindent::DEFAULT_CINKEYS),
            cinscopedecls: Cow::Borrowed(cindent::DEFAULT_CINSCOPEDECLS),
            joinspaces: false,
            wrapmargin: 0,
        },
        search_case: SearchCase {
            ignore: false,
            smart: false,
        },
        caret: CaretGravity::OnChar,
    };

    /// Set one `:set` option, clamped as [`EditorState::set_option`] documents.
    pub(crate) fn set(&mut self, opt: EditorOption) {
        match opt {
            EditorOption::IgnoreCase(v) => self.search_case.ignore = v,
            EditorOption::SmartCase(v) => self.search_case.smart = v,
            // Clamped to nvim's bounds (a C `int`; `'tabstop'` 1..=9999), so an API caller cannot make
            // `>>` or a `<Tab>` allocate without limit (#684); `:set` rejects a value past them first.
            EditorOption::ShiftWidth(n) => self.indent.shift_width = n.min(MAX_OPTION_INT),
            EditorOption::TabStop(n) => self.indent.tab_width = n.clamp(1, MAX_TABSTOP),
            EditorOption::ExpandTab(v) => {
                self.indent.style = if v {
                    IndentStyle::Space
                } else {
                    IndentStyle::Tab
                }
            }
            EditorOption::TextWidth(n) => self.text_width = n.min(MAX_OPTION_INT),
            EditorOption::VarTabStop(v) => {
                if let Some(v) = VarStops::parse(&v) {
                    self.indent.vts = v;
                }
            }
            EditorOption::VarSoftTabStop(v) => {
                if let Some(v) = VarStops::parse(&v) {
                    self.indent.vsts = v;
                }
            }
            EditorOption::SoftTabStop(n) => {
                self.indent.soft_tab = n.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            }
            EditorOption::AutoIndent(v) => self.indent.autoindent = v,
            EditorOption::SmartIndent(v) => self.indent.smartindent = v,
            EditorOption::CopyIndent(v) => self.indent.copyindent = v,
            EditorOption::PreserveIndent(v) => self.indent.preserveindent = v,
            EditorOption::SmartTab(v) => self.indent.smarttab = v,
            EditorOption::ShiftRound(v) => self.indent.shiftround = v,
            EditorOption::CaretGravity(g) => self.caret = g,
            EditorOption::FormatOptions(v) => self.format.options = Cow::Owned(v),
            EditorOption::Comments(v) => self.format.comments = Cow::Owned(v),
            EditorOption::CommentString(v) => self.format.commentstring = Cow::Owned(v),
            EditorOption::FormatListPat(v) => self.format.listpat = Cow::Owned(v),
            EditorOption::FormatPrg(v) => self.format.prg = Cow::Owned(v),
            EditorOption::EqualPrg(v) => self.format.equalprg = Cow::Owned(v),
            EditorOption::CinWords(v) => self.format.cinwords = Cow::Owned(v),
            EditorOption::JoinSpaces(v) => self.format.joinspaces = v,
            EditorOption::Lisp(v) => self.format.lisp = v,
            EditorOption::LispWords(v) => self.format.lispwords = Cow::Owned(v),
            EditorOption::CIndent(v) => self.format.cindent = v,
            EditorOption::CinOptions(v) => self.format.cinoptions = Cow::Owned(v),
            EditorOption::CinKeys(v) => self.format.cinkeys = Cow::Owned(v),
            EditorOption::CinScopeDecls(v) => self.format.cinscopedecls = Cow::Owned(v),
            EditorOption::WrapMargin(n) => {
                self.format.wrapmargin = n.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            }
        }
    }

    /// These values as the `:set`s that would install them (`ic`, `scs`, `ts`, `sw`, `et`, `tw`; `sw` is
    /// the raw value, `0` follows `ts`), then the caret gravity.
    pub(crate) fn editing_options(&self) -> Vec<EditorOption> {
        vec![
            EditorOption::IgnoreCase(self.search_case.ignore),
            EditorOption::SmartCase(self.search_case.smart),
            EditorOption::TabStop(self.indent.tab_width),
            EditorOption::ShiftWidth(self.indent.shift_width),
            EditorOption::ExpandTab(self.indent.style == IndentStyle::Space),
            EditorOption::TextWidth(self.text_width),
            EditorOption::SoftTabStop(self.indent.soft_tab),
            EditorOption::VarTabStop(stops_text(&self.indent.vts)),
            EditorOption::VarSoftTabStop(stops_text(&self.indent.vsts)),
            EditorOption::AutoIndent(self.indent.autoindent),
            EditorOption::SmartIndent(self.indent.smartindent),
            EditorOption::CopyIndent(self.indent.copyindent),
            EditorOption::PreserveIndent(self.indent.preserveindent),
            EditorOption::SmartTab(self.indent.smarttab),
            EditorOption::ShiftRound(self.indent.shiftround),
            EditorOption::CaretGravity(self.caret),
            EditorOption::FormatOptions(self.format.options.to_string()),
            EditorOption::Comments(self.format.comments.to_string()),
            EditorOption::CommentString(self.format.commentstring.to_string()),
            EditorOption::FormatListPat(self.format.listpat.to_string()),
            EditorOption::FormatPrg(self.format.prg.to_string()),
            EditorOption::EqualPrg(self.format.equalprg.to_string()),
            EditorOption::CinWords(self.format.cinwords.to_string()),
            EditorOption::JoinSpaces(self.format.joinspaces),
            EditorOption::WrapMargin(self.format.wrapmargin),
            EditorOption::Lisp(self.format.lisp),
            EditorOption::LispWords(self.format.lispwords.to_string()),
            EditorOption::CIndent(self.format.cindent),
            EditorOption::CinOptions(self.format.cinoptions.to_string()),
            EditorOption::CinKeys(self.format.cinkeys.to_string()),
            EditorOption::CinScopeDecls(self.format.cinscopedecls.to_string()),
        ]
    }

    /// Set the tabstop AND the shiftwidth to `tab_width`, and the indent style ([`EditorState::set_indent`]).
    pub(crate) fn set_indent(&mut self, tab_width: usize, style: IndentStyle) {
        let tab_width = tab_width.clamp(1, MAX_TABSTOP);
        self.indent = IndentConfig {
            tab_width,
            shift_width: tab_width,
            style,
            ..self.indent
        };
    }

    /// Set `'ignorecase'` and `'smartcase'`.
    pub(crate) fn set_search_case(&mut self, ignore: bool, smart: bool) {
        self.search_case = SearchCase { ignore, smart };
    }

    /// Set the caret gravity (D-050).
    pub(crate) fn set_caret(&mut self, caret: CaretGravity) {
        self.caret = caret;
    }
}

/// A stop list as its option value: the widths joined by `,`, empty when unset.
pub(crate) fn stops_text(v: &VarStops) -> String {
    let parts: Vec<String> = v.stops().iter().map(u16::to_string).collect();
    parts.join(",")
}

impl IndentConfig {
    /// The effective shiftwidth: `shift_width`, or when it is `0` the tabstop (Vim's `sw=0`), under
    /// `'vartabstop'` its first stop (nvim's `get_sw_value`, #1163).
    fn shift_width(self) -> usize {
        if self.shift_width == 0 {
            self.tabs().interval_width(0, false)
        } else {
            self.shift_width
        }
    }

    /// The tab stops (`'tabstop'`, `'vartabstop'`) this config lays a line out with (#1163).
    pub(crate) fn tabs(self) -> Tabs {
        Tabs::new(self.tab_width, self.vts)
    }

    /// The leading whitespace that spans `cols` columns in this style: `cols` spaces (`expandtab`), else as
    /// many `\t` as fit at the tab stops plus a spaces remainder (Vim's `noexpandtab` indent).
    fn whitespace(self, cols: usize) -> Vec<u8> {
        match self.style {
            IndentStyle::Space => vec![b' '; cols],
            IndentStyle::Tab => self.tabs().fill(cols),
        }
    }
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

/// The wanted column (Vim `curswant`), measured only when something reads it (#1231). Most commands
/// reset the column to the cursor's, but only vertical moves, block selections and a few edits ever read
/// it, and measuring walks the line from its start: on a multi-megabyte line that walk dominated every
/// keypress. As nvim (`w_set_curswant`), the column is set to "where the cursor is now" and measured on
/// first read. The pending form keeps what the measure reads (the buffer version, cursor, `'tabstop'` and
/// the tab-end rule), so it gives the value the eager measure would have given, whatever happens to the
/// buffer or the View before the read. Holding the buffer version is a refcount on the document's
/// `Arc`; it keeps at most one older version alive until the next command replaces the column.
#[derive(Clone)]
pub(crate) struct Want(WantKind);

#[derive(Clone)]
enum WantKind {
    /// A known column ([`range::MAXCOL`] after `$`).
    Col(usize),
    /// The column of `pos` in `text`, measured once on first read.
    At {
        text: std::sync::Arc<Vec<u8>>,
        pos: usize,
        ts: Tabs,
        tab_end: bool,
        col: std::sync::OnceLock<usize>,
    },
}

impl Want {
    pub(crate) const fn col(c: usize) -> Self {
        Want(WantKind::Col(c))
    }

    /// The column: measured now when pending ([`range::wanted_vcol`]), then remembered.
    pub(crate) fn get(&self) -> usize {
        match &self.0 {
            WantKind::Col(c) => *c,
            WantKind::At {
                text,
                pos,
                ts,
                tab_end,
                col,
            } => *col.get_or_init(|| range::wanted_vcol(text, *pos, *ts, *tab_end)),
        }
    }
}

/// `curswant == MAXCOL` and friends. A pending column is a measured position, never [`range::MAXCOL`], so
/// that test does not measure.
impl PartialEq<usize> for Want {
    fn eq(&self, other: &usize) -> bool {
        match &self.0 {
            WantKind::Col(c) => c == other,
            WantKind::At { .. } if *other == range::MAXCOL => false,
            WantKind::At { .. } => self.get() == *other,
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
    named_marks: [Option<usize>; 62],
    /// The one end of the last Visual area that is set (`'<'`, its start, or `'>'`, its end) when `:mark <`
    /// or `:mark >` made it with no Visual area before (#1241). Vim's other end is then unset: both `<` and
    /// `>` read the one that is (so [`View::last_visual`] holds it twice), but `gv` has nothing to reselect.
    visual_one_end: Option<char>,
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
    /// register (`"=<expr><CR>`). The carried string is the already-formatted result (EMPTY on an
    /// evaluation error); [`commit`] writes it into the slot and sets the pending register, then returns
    /// early exactly like [`Action::SetPending`] so the following `p`/`P` reads it.
    SetExprPending(String),
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
    /// The top row's length in bytes when Insert began (after a change's delete; nvim's `pre_textlen`):
    /// what a block change measures the text typed on it against when a `<CR>` ended that row (#1142).
    top_len: usize,
    /// Virtual (screen) column at which each lower row receives the typed text (left edge for `I`/`c`,
    /// right edge+1 for `A`).
    target_col: usize,
    /// Number of block rows below the top row (the rows to replicate onto).
    rows_below: usize,
    /// `A`/append semantics: pad rows shorter than `target_col` with spaces. `false` (`I`/`c`) skips a row
    /// that does not reach `target_col`.
    append: bool,
    /// `$`-ragged append (`` <C-v>$A ``): insert at EACH row's own line-end rather than a fixed `target_col`,
    /// so trailing text aligns to variable line lengths (Vim). Only meaningful with `append`.
    to_eol: bool,
    /// `c`/`s` (change) rather than `I`/`A` (insert/append). Only the cursor RESTING position differs: Vim
    /// leaves a block CHANGE with the caret one char left of the top row's typed text (normal Insert-exit),
    /// whereas `I`/`A` snap back to the block's top-left corner. The replicate geometry is identical.
    change: bool,
    /// The block's width in cells (`hi - lo + 1`, >= 1) — the number of columns `A` appends
    /// past and `c` deletes per row. Retained (with the row count) into [`BlockGeom`] so dot-repeat can
    /// rebuild an equivalent block at the caret. `I` ignores it (insert at the left edge), but it is stored
    /// uniformly so the geometry round-trips for every kind.
    width: usize,
}

/// The geometry of a committed blockwise insert-replicate (`CTRL-V I/A/c`), the minimum a dot-repeat needs
/// to reconstruct an equivalent block at a NEW caret: nvim positions the repeat's top-left at the cursor and
/// reuses the original `width` x `height` (verified vs nvim v0.12.4). `kind`/`to_eol` reproduce the I/A/c
/// variant and the `$`-ragged (`$A`) append. The caret column and row come from the live cursor at `.` time,
/// so only the SIZE and MODE are stored here — not the original position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BlockGeom {
    /// Total block rows (>= 1) — how many lines the repeat replicates onto, clamped at end-of-buffer.
    height: usize,
    /// The block's width in cells (>= 1) — what `A` appends past / `c` deletes per row (see
    /// [`BlockInsert::width`]).
    width: usize,
    /// `I` (insert at left edge) / `A` (append at right edge) / `c` (delete then insert at left edge).
    kind: BlockInsertKind,
    /// `$`-ragged append (`$A`): append at each row's own line-end rather than a fixed column. Only with `A`.
    to_eol: bool,
}

/// The AMOUNT of text the last Visual-selection operator covered, kept so dot-repeat (`.`) can rebuild an
/// equivalent selection at the caret (`:help visual-repeat`, #794): the same number of lines; within one
/// line the same number of screen cells; for a multi-line charwise selection the same end column on the
/// last line; for a block the same width. Columns are VIRTUAL (screen) columns, as in Vim (#1064): a `<Tab>`
/// spans to its tabstop and a wide character two cells (see [`vcols_at`](range::vcols_at)).
#[derive(Clone, Copy, Debug)]
struct VisualOpGeom {
    kind: SelectKind,
    /// Lines spanned (>= 1).
    lines: usize,
    /// Charwise one-line: cell count. Charwise multi-line: the end's last cell column. Blockwise: width.
    cols: usize,
    /// `$` extended the selection (curswant at MAXCOL): the repeat runs to each line's end.
    to_eol: bool,
}

/// Whether `cmd` operates on the live Visual/Select selection — the commands whose dot-repeat (issued in
/// Normal, with no selection) rebuilds one from [`VisualOpGeom`]. Block `I`/`A`/`c` has its own
/// [`BlockGeom`] path in the planner.
fn is_selection_op(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::DeleteSelection
            | Command::ChangeSelection
            | Command::ReplaceSelection(_)
            | Command::ReplaceSelectionChar(_)
            | Command::CaseSelection(_)
            | Command::PasteSelection { .. }
            | Command::IncrementSelection { .. }
            | Command::VisualOp(_)
    )
}

/// Before a selection operator runs: record the live selection's geometry, or — for a dot-repeat, which
/// arrives in Normal with no selection — rebuild one of the recorded size starting at the caret, so the
/// operator then plans exactly as if the user had re-selected that text (Vim's `redo_VIsual`).
fn prepare_selection_op(st: &mut EditorState, cmd: &Command) {
    if !is_selection_op(cmd) {
        return;
    }
    let b = st.doc.bytes();
    let cur = st.view.cursor;
    match (st.view.mode.selection(), st.view.anchor) {
        (Some(kind), Some(anchor)) => {
            let (lo, hi) = (anchor.min(cur), anchor.max(cur));
            let lines = crate::pos::raw::line_of(b, hi) - crate::pos::raw::line_of(b, lo) + 1;
            let ts = st.view.indent.tabs();
            let cols = match kind {
                SelectKind::Charwise if lines == 1 => {
                    vcols_at(b, hi, ts).1 + 1 - vcols_at(b, lo, ts).0
                }
                SelectKind::Charwise => vcols_at(b, hi, ts).1,
                SelectKind::Linewise => 0,
                SelectKind::Blockwise => block_geom(b, anchor, cur, ts, false).width(),
            };
            st.view.last_visual_op = Some(VisualOpGeom {
                kind,
                lines,
                cols,
                to_eol: st.view.curswant == range::MAXCOL,
            });
        }
        (None, _) => {
            let Some(g) = st.view.last_visual_op else {
                return;
            };
            let ts = st.view.indent.tabs();
            let ls = line_start(b, cur);
            let start_col = vcols_at(b, cur, ts).0;
            let end_ls =
                crate::pos::raw::nth_line_start(b, crate::pos::raw::line_of(b, cur) + g.lines - 1);
            let end = match g.kind {
                _ if g.to_eol => line_end(b, end_ls),
                SelectKind::Charwise if g.lines == 1 => at_vcol(b, ls, start_col + g.cols - 1, ts),
                SelectKind::Charwise => at_vcol(b, end_ls, g.cols, ts),
                SelectKind::Linewise => end_ls,
                SelectKind::Blockwise => at_vcol(b, end_ls, start_col + g.cols - 1, ts),
            };
            st.view.mode = Mode::Visual { kind: g.kind };
            st.view.anchor = Some(cur);
            st.view.cursor = end;
            if g.to_eol {
                st.view.curswant = Want::col(range::MAXCOL);
            }
        }
        (Some(_), None) => {}
    }
}

/// How a committed command should route its captured text into the register store. A `Yank` additionally
/// seeds the yank register `"0` (when unregistered); an `Edit` (delete/change/`x`) never touches `"0`, so
/// `"0` survives intervening deletes (Vim `:help quote0`).
enum RegWrite {
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

    /// Set the indentation config the shift operators (`>>`/`<<`) use. Runtime config loading is deferred;
    /// until it lands this is the seam a loader (or a test) uses to install `editor.tab_width` /
    /// `editor.indent_style`. Sets the tabstop AND the shiftwidth to `tab_width` (the common `ts=sw` case);
    /// `:set shiftwidth`/`:set tabstop` ([`EditorState::set_option`]) set them apart.
    pub fn set_indent(&mut self, tab_width: usize, indent_style: IndentStyle) {
        self.view
            .update_options(|o| o.set_indent(tab_width, indent_style));
    }

    /// Set the search case config (`'ignorecase'` / `'smartcase'`, F-009 #1). The seam a config loader
    /// (or a test) uses until runtime config lands; the default is Vim's factory case-sensitive search.
    pub fn set_search_case(&mut self, ignore_case: bool, smart_case: bool) {
        self.view
            .update_options(|o| o.set_search_case(ignore_case, smart_case));
    }

    /// Set one editor option (`:set`) on the focused view, leaving the others untouched. Maps the
    /// runtime `:set` surface onto the existing indent/search-case config (no new schema keys).
    pub fn set_option(&mut self, opt: EditorOption) {
        self.view.update_options(|o| o.set(opt));
    }

    /// After an edit applied outside [`commit`]: snap the cursor and every stored offset into the new
    /// buffer (#691). Ex edits bypass `commit`, so without this they leave offsets mid-character.
    fn settle_offsets(&mut self) {
        let bytes = self.doc.bytes();
        self.view.cursor = snap(bytes, self.view.cursor);
        self.view.snap_offsets(bytes);
    }

    /// The leading whitespace for an indent of `level` shiftwidths, in the configured style (spaces, or
    /// tabs at the tabstop plus a spaces remainder) — Vim's `set_indent(level * sw)`.
    fn indent_for_level(&self, level: usize) -> Vec<u8> {
        self.view
            .indent
            .whitespace(level * self.view.indent.shift_width())
    }

    /// The effective shiftwidth in columns (see [`View::shift_width`]).
    fn shift_width(&self) -> usize {
        self.view.indent.shift_width()
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

    /// The 0-based inclusive line range the operator span of `cmd` covers — a search, `n`/`N`, `*`/`#`, a
    /// `gn` match or a mark — or `None` when it has no target or is not a span command. Read-only; the
    /// command's operator is ignored. The `!` filter numbers its `:{range}!` line from this (#800).
    #[must_use]
    pub fn span_lines(&self, cmd: &Command) -> Option<(usize, usize)> {
        planner::span_lines(self, cmd)
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

    /// Sync the four read-only special registers `"/ ": ". "%` from frontend state ahead of a
    /// register-reading command, so `"/p` / `C-r :` / `".p` / `"%p` resolve to the live values (see
    /// [`RegisterStore::set_special`](crate::register::RegisterStore::set_special)).
    pub fn set_special_registers(
        &mut self,
        search: Option<String>,
        last_ex: Option<String>,
        inserted: Option<String>,
        file: Option<String>,
    ) {
        self.view
            .registers
            .set_special(search, last_ex, inserted, file);
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
    pub fn selection_span(&self) -> Option<(BytePos, BytePos)> {
        // Visual and Select paint the same selection (they share the anchor and toggle via CTRL-G).
        self.view.selection_span(self.bytes())
    }

    /// The per-row highlighted byte ranges of the current BLOCKWISE selection (one `[start, end)` per line
    /// the rectangle crosses; short lines contribute an empty range at their end), or `None` when the
    /// selection is not blockwise. For the frontend to paint a column-aligned block.
    #[must_use]
    pub fn block_spans(&self) -> Option<Vec<(BytePos, BytePos)>> {
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
    pub fn cursor(&self) -> BytePos {
        BytePos(self.view.cursor)
    }

    /// The jumplist positions (oldest → newest) for `:jumps`.
    #[must_use]
    pub fn jumps_snapshot(&self) -> Vec<BytePos> {
        self.view.jumps.iter().copied().map(BytePos).collect()
    }

    /// The change-list positions (oldest → newest) for `:changes`.
    #[must_use]
    pub fn changes_snapshot(&self) -> Vec<BytePos> {
        self.view.changes.iter().copied().map(BytePos).collect()
    }

    /// The SET marks for `:marks` — the named marks `a`–`z`, the `.` last-change mark, and the `^`
    /// last-insert mark — as `(name, byte offset)`, in that order (unset marks omitted).
    #[must_use]
    pub fn marks_snapshot(&self) -> Vec<(char, BytePos)> {
        let mut out = Vec::new();
        for c in ('a'..='z').chain('A'..='Z').chain('0'..='9') {
            if let Some(p) = self.view.named_mark(c) {
                out.push((c, p));
            }
        }
        if let Some(p) = self.view.last_change() {
            out.push(('.', p));
        }
        if let Some(p) = self.view.last_insert() {
            out.push(('^', BytePos(p)));
        }
        out
    }

    /// The mark's byte offset, or `None` when no mark is set. The mark is the Emacs profile's other end of
    /// the region (`set-mark-command` and the region ops); one-caret degenerate is `None`. View-local. For
    /// the Emacs parity comparator and a future `:marks`.
    #[must_use]
    pub fn mark(&self) -> Option<BytePos> {
        self.view.mark.map(BytePos)
    }

    /// Place the cursor at byte offset `pos` (on a char boundary). View-local; does not touch the document.
    /// Used to home a fixture's starting point in the parity comparator (mirrors the frontend seam).
    ///
    /// Placing the caret also SEEDS the sticky desired column (`curswant`) to `pos`'s column, exactly as a
    /// committed motion would: a subsequent `j`/`k` (`next-line`/`previous-line`) aims at the column the
    /// caret was placed on, not column 0. Without this a teleport (goto, mouse, a fixture's start point)
    /// would leave `curswant` stale at 0 and vertical moves would snap to the line start.
    pub fn set_cursor(&mut self, pos: BytePos) {
        let pos = pos.0;
        let b = self.doc.bytes();
        // Clamp to the buffer and snap to a char boundary: `set_cursor` is a public seam (tests, a
        // future goto/mouse), so an out-of-range or mid-codepoint `pos` must not leave the cursor where
        // a later `col_of`/slice would panic. Vim never overshoots, so this is a no-op on valid input.
        let pos = snap(b, pos.min(b.len()));
        self.view.cursor = pos;
        self.view.curswant = self.wanted_col();
    }

    /// The wanted column ([`View::curswant`]) the caret sets where it stands now (#1090), pending: it is
    /// measured when first read ([`Want`], #1231).
    fn wanted_col(&self) -> Want {
        let tab_end = match self.view.mode {
            Mode::Normal => self.view.caret == CaretGravity::OnChar,
            Mode::Visual { .. } => self.view.anchor.is_none_or(|a| self.view.cursor >= a),
            _ => false,
        };
        Want(WantKind::At {
            text: self.doc.text_arc(),
            pos: self.view.cursor,
            ts: self.view.indent.tabs(),
            tab_end,
            col: std::sync::OnceLock::new(),
        })
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
use search::{
    landing_key, match_spans, search_bwd, search_fwd, search_spans, step_char, walk_count,
};

mod line_cmds;
mod substitute;
/// The options a `:sort` carries (parsed by the frontend from `[!][i][n][r][u] [/pattern/]`). Grouped into
/// one struct so [`EditorState::sort_lines`] and the `Workspace` wrapper take a single argument.
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
