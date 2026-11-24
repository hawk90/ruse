//! The editor options a [`View`](super::View) carries: the `:set` surface ([`EditorOption`]), the indent,
//! search-case and text-formatting config behind it, their grouping into [`ViewOptions`], and the caret
//! gravity the input profile selects. Moved out of `editor/mod.rs` (#1495); the parent re-exports every
//! item, so the old paths resolve.

use std::borrow::Cow;

use super::{cindent, format, indent, lisp, OptionValueError};
use crate::cells::{Tabs, VarStops};

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
    /// `'wrapscan'` (`ws`, Vim-global) — a search wraps around the end (or, backward, the start) of the
    /// buffer. Off, a search that finds nothing before the edge fails (E385 / E384, #1558).
    WrapScan(bool),
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
                | EditorOption::WrapScan(_)
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
/// concern is one field on [`View`](super::View), not four loose ones.
#[derive(Clone, Copy)]
pub(super) struct IndentConfig {
    /// `editor.tab_width` / `'tabstop'` — the columns one `\t` spans. Schema default 4. Always `>= 1`.
    pub(super) tab_width: usize,
    /// `editor.shift_width` / `'shiftwidth'` — one indent level's width in columns. `0` follows
    /// [`IndentConfig::tab_width`], as Vim's `sw=0` does. Schema default 4.
    pub(super) shift_width: usize,
    /// `editor.indent_style` — whether an indent level is spaces or a tab. Schema default `space`.
    pub(super) style: IndentStyle,
    /// `'softtabstop'`: `0` off, negative follows the effective shiftwidth (`editor.softtabstop`).
    pub(super) soft_tab: i64,
    /// `'vartabstop'` (`vts`): the widths of the successive tab stops; wins over `tab_width` when set
    /// (#1163).
    pub(super) vts: VarStops,
    /// `'varsofttabstop'` (`vsts`): the widths of the successive soft tab stops; wins over `soft_tab`
    /// when set (#1163).
    pub(super) vsts: VarStops,
    /// `'autoindent'` (`editor.autoindent`, default on as in nvim).
    pub(super) autoindent: bool,
    /// `'smartindent'` (`editor.smartindent`).
    pub(super) smartindent: bool,
    /// `'copyindent'` (`editor.copyindent`).
    pub(super) copyindent: bool,
    /// `'preserveindent'` (`editor.preserveindent`).
    pub(super) preserveindent: bool,
    /// `'smarttab'` (`editor.smarttab`, default on as in nvim). Vim-global.
    pub(super) smarttab: bool,
    /// `'shiftround'` (`editor.shiftround`). Vim-global.
    pub(super) shiftround: bool,
}

/// The search config: `'ignorecase'` and `'smartcase'` (F-009), default off — the factory Vim default the
/// differential oracle runs against — and `'wrapscan'`, default on as in Vim (#1558).
#[derive(Clone, Copy)]
pub(super) struct SearchCase {
    pub(super) ignore: bool,
    pub(super) smart: bool,
    pub(super) wrap: bool,
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

/// The option values a [`View`](super::View) carries (indent, `'textwidth'`, formatting, search case,
/// caret gravity), grouped so the Workspace can keep the GLOBAL values Vim's `:set` also writes and hand
/// them to every View it creates (`:enew`, `:e`, `:bd`'s replacement), as Vim copies global options into a
/// new buffer (#754).
#[derive(Clone)]
pub(crate) struct ViewOptions {
    pub(super) indent: IndentConfig,
    pub(super) text_width: usize,
    pub(super) format: FormatConfig,
    pub(super) search_case: SearchCase,
    pub(super) caret: CaretGravity,
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
            wrap: true,
        },
        caret: CaretGravity::OnChar,
    };

    /// Set one `:set` option, clamped as [`EditorState::set_option`](super::EditorState::set_option) documents.
    pub(crate) fn set(&mut self, opt: EditorOption) {
        match opt {
            EditorOption::IgnoreCase(v) => self.search_case.ignore = v,
            EditorOption::SmartCase(v) => self.search_case.smart = v,
            EditorOption::WrapScan(v) => self.search_case.wrap = v,
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
            EditorOption::WrapScan(self.search_case.wrap),
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

    /// Set the tabstop AND the shiftwidth to `tab_width`, and the indent style ([`EditorState::set_indent`](super::EditorState::set_indent)).
    pub(crate) fn set_indent(&mut self, tab_width: usize, style: IndentStyle) {
        let tab_width = tab_width.clamp(1, MAX_TABSTOP);
        self.indent = IndentConfig {
            tab_width,
            shift_width: tab_width,
            style,
            ..self.indent
        };
    }

    /// Set `'ignorecase'` and `'smartcase'` (`'wrapscan'` is kept).
    pub(crate) fn set_search_case(&mut self, ignore: bool, smart: bool) {
        self.search_case = SearchCase {
            ignore,
            smart,
            ..self.search_case
        };
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
    pub(super) fn shift_width(self) -> usize {
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
    pub(super) fn whitespace(self, cols: usize) -> Vec<u8> {
        match self.style {
            IndentStyle::Space => vec![b' '; cols],
            IndentStyle::Tab => self.tabs().fill(cols),
        }
    }
}

/// How a caret rests relative to text (D-050 / RFC-0015). A View-local property SELECTED BY THE INPUT
/// PROFILE, orthogonal to [`Mode`](super::Mode): it decides whether the line-/buffer-end position is the last character
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
