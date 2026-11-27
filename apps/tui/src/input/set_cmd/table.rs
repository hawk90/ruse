//! The options ruse honours: their kinds, scopes, defaults and validation sets (`OPTIONS`). Split from `set_cmd.rs` (#1388).

use super::Cow;

/// An option ruse honours.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Opt {
    AutoIndent,
    CopyIndent,
    PreserveIndent,
    ShiftRound,
    SmartIndent,
    SmartTab,
    SoftTabStop,
    VarTabStop,
    VarSoftTabStop,
    IgnoreCase,
    SmartCase,
    WrapScan,
    TabStop,
    ShiftWidth,
    ExpandTab,
    TextWidth,
    HlSearch,
    IncSearch,
    FixEol,
    TildeOp,
    ScrollOff,
    FoldEnable,
    FoldMethod,
    FoldLevel,
    Comments,
    CommentString,
    FormatListPat,
    FormatOptions,
    FormatPrg,
    EqualPrg,
    CinWords,
    Lisp,
    LispWords,
    CIndent,
    CinOptions,
    CinKeys,
    CinScopeDecls,
    JoinSpaces,
    WrapMargin,
    CpOptions,
    History,
    ShowMode,
    Verbose,
    VerboseFile,
    BreakAt,
    BreakIndent,
    BreakIndentOptions,
    Display,
    LineBreak,
    List,
    ListChars,
    Number,
    NumberWidth,
    RelativeNumber,
    ScrollJump,
    ShowBreak,
    SideScroll,
    SideScrollOff,
    SignColumn,
    Wrap,
    ColorColumn,
    CursorColumn,
    CursorLine,
    CursorLineOptions,
    FillChars,
    FoldColumn,
    Scroll,
    SmoothScroll,
    /// `'readonly'`: `:w` refuses (E45) and the first change warns (W10). Per buffer (#1211).
    ReadOnly,
    /// `'messagesopt'`: its `history:` item sizes the `:messages` history (#1235); the other items are
    /// checked and kept, and ruse always behaves as `hit-enter`.
    MessageOptions,
    /// `'foldopen'`: which typed commands open the closed fold the cursor lands in (#1141).
    FoldOpen,
    /// `'report'`: a command that changes, yanks or moves more lines than this says how many (#1276).
    Report,
    /// `'cmdheight'`: the message rows below the bar (RFC-0027 §1); 0 shares the bar's row.
    CmdHeight,
    /// The write options (#619): `app/write_opts.rs` holds their values.
    Backup,
    BackupCopy,
    BackupDir,
    BackupExt,
    BackupSkip,
    PatchMode,
    WriteBackup,
    Write,
    WriteAny,
    Fsync,
    /// Per buffer, the file's own shape: a final newline, a DOS CTRL-Z, a BOM.
    EndOfLine,
    EndOfFile,
    Bomb,
    /// The buffer-lifecycle options (#619, `app/abandon.rs`).
    AutoRead,
    AutoWrite,
    AutoWriteAll,
    Confirm,
    Hidden,
    /// Per buffer: whether commands may change the text, and whether it differs from the file.
    Modifiable,
    Modified,
    /// Per buffer: the line ending a write uses, and `'binary'` (#619).
    FileFormat,
    Binary,
    /// The line endings a read tries (#619).
    FileFormats,
    /// The recovery journal as Vim's swap file (#619, RFC-0023): per buffer, how often it syncs, and
    /// where it lives (read-only: ruse's journal directory).
    SwapFile,
    UpdateCount,
    Directory,
    /// `'undolevels'`: how many undo levels every buffer keeps, with the `editor.undo_max_bytes` budget
    /// (RFC-0026 §1, #1290). Negative: no undo; 0: Vi's `u` that undoes itself.
    UndoLevels,
}

/// An option's value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OptVal {
    Bool(bool),
    Num(i64),
    /// A string option's value: always one of its [`OptKind::Str`] values.
    Str(&'static str),
    /// A free-form string option's value ([`OptKind::Text`]).
    Text(Cow<'static, str>),
}

impl OptVal {
    const fn text(s: &'static str) -> OptVal {
        OptVal::Text(Cow::Borrowed(s))
    }
}

/// How `+=` / `-=` / `^=` treat a free-form string option (nvim's `P_FLAGLIST` / `P_COMMA`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListKind {
    /// A plain string: append, remove the first occurrence, prepend.
    Plain,
    /// One-letter flags (`'formatoptions'`): `+=` / `^=` drop the flags already there first, unless the
    /// whole value is already in the option.
    Flags,
    /// A comma-separated list (`'comments'`): items are added with a comma, unless present, and removed
    /// whole.
    Comma,
}

/// The check a free-form string value must pass (nvim's `did_set_*` callbacks).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextCheck {
    None,
    FormatOptions,
    Comments,
    CommentString,
    ListChars,
    BreakIndentOpt,
    ShowBreak,
    Display,
    SignColumn,
    FillChars,
    CursorLineOpt,
    ColorColumn,
    FoldColumn,
    MessagesOpt,
    FoldOpen,
    /// `'vartabstop'` / `'varsofttabstop'`: positive widths separated by commas (`E474`, #1163).
    TabStops,
    /// `'backupcopy'`: one of `yes` / `no` / `auto`, and `breaksymlink` / `breakhardlink` (`E474`).
    BackupCopy,
    /// `'backupext'` / `'patchmode'`: no `/` (`E474`).
    BackupExt,
    /// `'fileformats'`: `unix`, `dos`, `mac`, comma-separated (`E474`).
    FileFormats,
}

/// An option's type, with its value rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptKind {
    Bool,
    /// A number, at least `min` (below it: `E487`) and at most `max` (above it: `E474`). Every number
    /// is a C `int` in nvim, so `max` never exceeds `i32::MAX`.
    Num {
        min: i64,
        max: i64,
    },
    /// One of `values` (anything else: `E474`), of which ruse implements `supported`.
    Str {
        values: &'static [&'static str],
        supported: &'static [&'static str],
    },
    /// Any string passing `check`, combined by `list`.
    Text {
        list: ListKind,
        check: TextCheck,
    },
}

/// Where an option's value lives in ruse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Locality {
    /// One value (Vim-global, or one ruse does not keep per window or buffer).
    Global,
    /// A local value on each View plus a global one new Views take (`ts`, `sw`, `et`, `tw`).
    Local,
    /// A value per buffer and no separate global one (the fold options).
    PerBuffer,
    /// nvim's global-local window option (`'scrolloff'`): a global value, and a local one per window
    /// that is unset (`-1`) until `:setlocal` gives it one; `:set` sets the global value and unsets the
    /// focused window's.
    GlobalLocal,
}

/// One option ruse honours.
#[derive(Clone, Debug)]
pub struct OptDef {
    pub opt: Opt,
    pub name: &'static str,
    pub short: &'static str,
    pub kind: OptKind,
    /// Vim's (nvim v0.12.6's) default, which `opt&vim` restores.
    pub vim_default: OptVal,
    /// ruse's default (`spec/config-schema.yaml`), which `opt&` restores and `:set` compares with.
    pub default: OptVal,
    pub locality: Locality,
}

pub(super) const INT_MAX: i64 = i32::MAX as i64;

pub(super) const INT_MIN: i64 = i32::MIN as i64;

/// nvim's largest `'history'` (`MAX_HIST_LEN`): more is `E474`.
pub const MAX_HISTORY: i64 = 10_000;

/// nvim's default `'comments'` and `'formatlistpat'`.
const COMMENTS: &str = "s1:/*,mb:*,ex:*/,://,b:#,:%,:XCOMM,n:>,fb:-,fb:•";

const LISTPAT: &str = r"^\s*\d\+[\]:.)}\t ]\s*";

/// nvim's default `'cpoptions'`: the only value ruse implements (it behaves as these flags say).
pub const CPO_DEFAULT: &str = "aABceFs_";

/// nvim's default `'foldopen'`.
pub const FOLDOPEN: &str = "block,hor,mark,percent,quickfix,search,tag,undo";

/// The items a `'foldopen'` value may hold (nvim's `p_fdo_values`); any other is `E474`.
pub const FOLDOPEN_ITEMS: &[&str] = &[
    "all", "block", "hor", "mark", "percent", "quickfix", "search", "tag", "insert", "undo", "jump",
];

/// Whether `value` is a `'foldopen'` nvim takes: comma-separated [`FOLDOPEN_ITEMS`].
#[must_use]
pub fn foldopen_valid(value: &str) -> bool {
    value
        .split(',')
        .all(|i| i.is_empty() || FOLDOPEN_ITEMS.contains(&i))
}

/// Whether `value` is a `'backupcopy'` nvim takes: exactly one of `yes`, `no`, `auto`, and any of
/// `breaksymlink`, `breakhardlink`, comma-separated.
#[must_use]
pub fn backupcopy_valid(value: &str) -> bool {
    let mut method = 0;
    for item in value.split(',') {
        match item {
            "yes" | "no" | "auto" => method += 1,
            "breaksymlink" | "breakhardlink" => {}
            _ => return false,
        }
    }
    method == 1
}

/// nvim's default `'messagesopt'`.
pub const MESSAGESOPT: &str = "hit-enter,history:500,progress:c";

/// The `history:` size a `'messagesopt'` value gives, `None` when nvim refuses the value (`E474`): items
/// `hit-enter`, `wait:{n}`, `history:{n}` and `progress:` / `progress:c`, separated by commas; `history:`
/// must be there, and `hit-enter` or `wait:`; a number is digits only, at most 10000.
#[must_use]
pub fn messagesopt_history(value: &str) -> Option<usize> {
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<usize>().ok())
            .flatten()
            .filter(|&n| n <= 10_000)
    };
    let (mut history, mut wait, mut hit_enter) = (None, false, false);
    for item in value.split(',') {
        if item.is_empty() {
            continue;
        }
        if item == "hit-enter" {
            hit_enter = true;
        } else if let Some(n) = item.strip_prefix("history:") {
            history = Some(number(n)?);
        } else if let Some(n) = item.strip_prefix("wait:") {
            number(n)?;
            wait = true;
        } else {
            // `progress:` takes nothing or `c`; anything else is no item.
            item.strip_prefix("progress:")
                .filter(|p| matches!(*p, "" | "c"))?;
        }
    }
    history.filter(|_| hit_enter || wait)
}

/// nvim's default `'breakat'` and `'listchars'`.
const BREAKAT: &str = crate::input::display_opts::BREAKAT_DEFAULT;

const LISTCHARS: &str = crate::input::display_opts::LISTCHARS_DEFAULT;

const FOLD_METHODS: &[&str] = &["manual", "expr", "marker", "syntax", "diff", "indent"];

/// `'fileformat'`'s values.
pub(super) const FILE_FORMATS: &[&str] = &["unix", "dos", "mac"];

const fn def(
    opt: Opt,
    name: &'static str,
    short: &'static str,
    kind: OptKind,
    vim_default: OptVal,
    default: OptVal,
    locality: Locality,
) -> OptDef {
    OptDef {
        opt,
        name,
        short,
        kind,
        vim_default,
        default,
        locality,
    }
}

const NUM0: OptKind = OptKind::Num {
    min: 0,
    max: INT_MAX,
};

/// Every option ruse honours, in name order.
pub const OPTIONS: &[OptDef] = &[
    def(
        Opt::AutoIndent,
        "autoindent",
        "ai",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Local,
    ),
    def(
        Opt::AutoRead,
        "autoread",
        "ar",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::AutoWrite,
        "autowrite",
        "aw",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::AutoWriteAll,
        "autowriteall",
        "awa",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::Confirm,
        "confirm",
        "cf",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::Hidden,
        "hidden",
        "hid",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::Modifiable,
        "modifiable",
        "ma",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::PerBuffer,
    ),
    def(
        Opt::Modified,
        "modified",
        "mod",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::PerBuffer,
    ),
    def(
        Opt::Binary,
        "binary",
        "bin",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::PerBuffer,
    ),
    def(
        Opt::FileFormat,
        "fileformat",
        "ff",
        OptKind::Str {
            values: FILE_FORMATS,
            supported: FILE_FORMATS,
        },
        OptVal::Str("unix"),
        OptVal::Str("unix"),
        Locality::PerBuffer,
    ),
    def(
        Opt::FileFormats,
        "fileformats",
        "ffs",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::FileFormats,
        },
        OptVal::text("unix,dos"),
        OptVal::text("unix,dos"),
        Locality::Global,
    ),
    def(
        Opt::Directory,
        "directory",
        "dir",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::SwapFile,
        "swapfile",
        "swf",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::PerBuffer,
    ),
    def(
        Opt::UpdateCount,
        "updatecount",
        "uc",
        NUM0,
        OptVal::Num(200),
        OptVal::Num(200),
        Locality::Global,
    ),
    def(
        Opt::Backup,
        "backup",
        "bk",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::BackupCopy,
        "backupcopy",
        "bkc",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::BackupCopy,
        },
        OptVal::text("auto"),
        OptVal::text("auto"),
        Locality::Global,
    ),
    def(
        Opt::BackupDir,
        "backupdir",
        "bdir",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::BackupExt,
        "backupext",
        "bex",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::BackupExt,
        },
        OptVal::text("~"),
        OptVal::text("~"),
        Locality::Global,
    ),
    def(
        Opt::BackupSkip,
        "backupskip",
        "bsk",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::Bomb,
        "bomb",
        "bomb",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::PerBuffer,
    ),
    def(
        Opt::BreakAt,
        "breakat",
        "brk",
        OptKind::Text {
            list: ListKind::Flags,
            check: TextCheck::None,
        },
        OptVal::text(BREAKAT),
        OptVal::text(BREAKAT),
        Locality::Global,
    ),
    def(
        Opt::BreakIndent,
        "breakindent",
        "bri",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::BreakIndentOptions,
        "breakindentopt",
        "briopt",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::BreakIndentOpt,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Local,
    ),
    def(
        Opt::CinWords,
        "cinwords",
        "cinw",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text("if,else,while,do,for,switch"),
        OptVal::text("if,else,while,do,for,switch"),
        Locality::Local,
    ),
    def(
        Opt::CmdHeight,
        "cmdheight",
        "ch",
        NUM0,
        OptVal::Num(1),
        OptVal::Num(1),
        Locality::Global,
    ),
    def(
        Opt::ColorColumn,
        "colorcolumn",
        "cc",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::ColorColumn,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Local,
    ),
    def(
        Opt::Comments,
        "comments",
        "com",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::Comments,
        },
        OptVal::text(COMMENTS),
        OptVal::text(COMMENTS),
        Locality::Local,
    ),
    def(
        Opt::CommentString,
        "commentstring",
        "cms",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::CommentString,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Local,
    ),
    def(
        Opt::CopyIndent,
        "copyindent",
        "ci",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::CpOptions,
        "cpoptions",
        "cpo",
        OptKind::Text {
            list: ListKind::Flags,
            check: TextCheck::None,
        },
        OptVal::text(CPO_DEFAULT),
        OptVal::text(CPO_DEFAULT),
        Locality::Global,
    ),
    def(
        Opt::CursorColumn,
        "cursorcolumn",
        "cuc",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::CursorLine,
        "cursorline",
        "cul",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::CursorLineOptions,
        "cursorlineopt",
        "culopt",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::CursorLineOpt,
        },
        OptVal::text("both"),
        OptVal::text("both"),
        Locality::Local,
    ),
    def(
        Opt::Display,
        "display",
        "dy",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::Display,
        },
        OptVal::text("lastline"),
        OptVal::text("lastline"),
        Locality::Global,
    ),
    def(
        Opt::EndOfFile,
        "endoffile",
        "eof",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::PerBuffer,
    ),
    def(
        Opt::EndOfLine,
        "endofline",
        "eol",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::PerBuffer,
    ),
    def(
        Opt::EqualPrg,
        "equalprg",
        "ep",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::ExpandTab,
        "expandtab",
        "et",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(true),
        Locality::Local,
    ),
    def(
        Opt::FillChars,
        "fillchars",
        "fcs",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::FillChars,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::GlobalLocal,
    ),
    def(
        Opt::FixEol,
        "fixendofline",
        "fixeol",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::FoldColumn,
        "foldcolumn",
        "fdc",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::FoldColumn,
        },
        OptVal::text("0"),
        OptVal::text("0"),
        Locality::Local,
    ),
    def(
        Opt::FoldEnable,
        "foldenable",
        "fen",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::PerBuffer,
    ),
    def(
        Opt::FoldLevel,
        "foldlevel",
        "fdl",
        NUM0,
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::PerBuffer,
    ),
    def(
        Opt::FoldMethod,
        "foldmethod",
        "fdm",
        OptKind::Str {
            values: FOLD_METHODS,
            supported: &["manual", "indent"],
        },
        OptVal::Str("manual"),
        OptVal::Str("manual"),
        Locality::PerBuffer,
    ),
    def(
        Opt::FormatListPat,
        "formatlistpat",
        "flp",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::None,
        },
        OptVal::text(LISTPAT),
        OptVal::text(LISTPAT),
        Locality::Local,
    ),
    def(
        Opt::FormatOptions,
        "formatoptions",
        "fo",
        OptKind::Text {
            list: ListKind::Flags,
            check: TextCheck::FormatOptions,
        },
        OptVal::text("tcqj"),
        OptVal::text("tcqj"),
        Locality::Local,
    ),
    def(
        Opt::FormatPrg,
        "formatprg",
        "fp",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::Fsync,
        "fsync",
        "fs",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::History,
        "history",
        "hi",
        OptKind::Num {
            min: 0,
            max: MAX_HISTORY,
        },
        OptVal::Num(MAX_HISTORY),
        OptVal::Num(MAX_HISTORY),
        Locality::Global,
    ),
    def(
        Opt::HlSearch,
        "hlsearch",
        "hls",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::IgnoreCase,
        "ignorecase",
        "ic",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::IncSearch,
        "incsearch",
        "is",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::WrapScan,
        "wrapscan",
        "ws",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::JoinSpaces,
        "joinspaces",
        "js",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::Lisp,
        "lisp",
        "lisp",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::LispWords,
        "lispwords",
        "lw",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(ruse_core::DEFAULT_LISPWORDS),
        OptVal::text(ruse_core::DEFAULT_LISPWORDS),
        Locality::Local,
    ),
    def(
        Opt::CIndent,
        "cindent",
        "cin",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::CinOptions,
        "cinoptions",
        "cino",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Local,
    ),
    def(
        Opt::CinKeys,
        "cinkeys",
        "cink",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(ruse_core::DEFAULT_CINKEYS),
        OptVal::text(ruse_core::DEFAULT_CINKEYS),
        Locality::Local,
    ),
    def(
        Opt::CinScopeDecls,
        "cinscopedecls",
        "cinsd",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::None,
        },
        OptVal::text(ruse_core::DEFAULT_CINSCOPEDECLS),
        OptVal::text(ruse_core::DEFAULT_CINSCOPEDECLS),
        Locality::Local,
    ),
    def(
        Opt::LineBreak,
        "linebreak",
        "lbr",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::List,
        "list",
        "list",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::ListChars,
        "listchars",
        "lcs",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::ListChars,
        },
        OptVal::text(LISTCHARS),
        OptVal::text(LISTCHARS),
        Locality::GlobalLocal,
    ),
    def(
        Opt::FoldOpen,
        "foldopen",
        "fdo",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::FoldOpen,
        },
        OptVal::text(FOLDOPEN),
        OptVal::text(FOLDOPEN),
        Locality::Global,
    ),
    def(
        Opt::MessageOptions,
        "messagesopt",
        "mopt",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::MessagesOpt,
        },
        OptVal::text(MESSAGESOPT),
        OptVal::text(MESSAGESOPT),
        Locality::Global,
    ),
    def(
        Opt::Number,
        "number",
        "nu",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::NumberWidth,
        "numberwidth",
        "nuw",
        OptKind::Num { min: 1, max: 20 },
        OptVal::Num(4),
        OptVal::Num(4),
        Locality::Local,
    ),
    def(
        Opt::PatchMode,
        "patchmode",
        "pm",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::BackupExt,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::PreserveIndent,
        "preserveindent",
        "pi",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::ReadOnly,
        "readonly",
        "ro",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::PerBuffer,
    ),
    def(
        Opt::RelativeNumber,
        "relativenumber",
        "rnu",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::Report,
        "report",
        "report",
        NUM0,
        OptVal::Num(2),
        OptVal::Num(2),
        Locality::Global,
    ),
    def(
        Opt::Scroll,
        "scroll",
        "scr",
        NUM0,
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::Local,
    ),
    def(
        Opt::ScrollJump,
        "scrolljump",
        "sj",
        OptKind::Num {
            min: -100,
            max: INT_MAX,
        },
        OptVal::Num(1),
        OptVal::Num(1),
        Locality::Global,
    ),
    def(
        Opt::ScrollOff,
        "scrolloff",
        "so",
        OptKind::Num {
            min: INT_MIN,
            max: INT_MAX,
        },
        OptVal::Num(0),
        OptVal::Num(3),
        Locality::GlobalLocal,
    ),
    def(
        Opt::ShiftRound,
        "shiftround",
        "sr",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::ShiftWidth,
        "shiftwidth",
        "sw",
        NUM0,
        OptVal::Num(8),
        OptVal::Num(4),
        Locality::Local,
    ),
    def(
        Opt::ShowBreak,
        "showbreak",
        "sbr",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::ShowBreak,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::GlobalLocal,
    ),
    def(
        Opt::ShowMode,
        "showmode",
        "smd",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::SideScroll,
        "sidescroll",
        "ss",
        NUM0,
        OptVal::Num(1),
        OptVal::Num(1),
        Locality::Global,
    ),
    def(
        Opt::SideScrollOff,
        "sidescrolloff",
        "siso",
        NUM0,
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::GlobalLocal,
    ),
    def(
        Opt::SignColumn,
        "signcolumn",
        "scl",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::SignColumn,
        },
        OptVal::text("auto"),
        OptVal::text("auto"),
        Locality::Local,
    ),
    def(
        Opt::SmartCase,
        "smartcase",
        "scs",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::SmartIndent,
        "smartindent",
        "si",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::SmartTab,
        "smarttab",
        "sta",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::SmoothScroll,
        "smoothscroll",
        "sms",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Local,
    ),
    def(
        Opt::VarTabStop,
        "vartabstop",
        "vts",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::TabStops,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Local,
    ),
    def(
        Opt::VarSoftTabStop,
        "varsofttabstop",
        "vsts",
        OptKind::Text {
            list: ListKind::Comma,
            check: TextCheck::TabStops,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Local,
    ),
    def(
        Opt::SoftTabStop,
        "softtabstop",
        "sts",
        OptKind::Num {
            min: INT_MIN,
            max: INT_MAX,
        },
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::Local,
    ),
    def(
        Opt::TabStop,
        "tabstop",
        "ts",
        OptKind::Num {
            min: 1,
            max: ruse_core::editor::MAX_TABSTOP as i64,
        },
        OptVal::Num(8),
        OptVal::Num(4),
        Locality::Local,
    ),
    def(
        Opt::TextWidth,
        "textwidth",
        "tw",
        NUM0,
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::Local,
    ),
    def(
        Opt::TildeOp,
        "tildeop",
        "top",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    // RFC-0026 slice 1 (#1290): the undo bound. Global here; nvim's is global-local.
    def(
        Opt::UndoLevels,
        "undolevels",
        "ul",
        OptKind::Num {
            min: INT_MIN,
            max: INT_MAX,
        },
        OptVal::Num(1000),
        OptVal::Num(1000),
        Locality::Global,
    ),
    def(
        Opt::Verbose,
        "verbose",
        "vbs",
        OptKind::Num {
            min: INT_MIN,
            max: INT_MAX,
        },
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::Global,
    ),
    def(
        Opt::VerboseFile,
        "verbosefile",
        "vfile",
        OptKind::Text {
            list: ListKind::Plain,
            check: TextCheck::None,
        },
        OptVal::text(""),
        OptVal::text(""),
        Locality::Global,
    ),
    def(
        Opt::Wrap,
        "wrap",
        "wrap",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Local,
    ),
    def(
        Opt::WrapMargin,
        "wrapmargin",
        "wm",
        OptKind::Num {
            min: INT_MIN,
            max: INT_MAX,
        },
        OptVal::Num(0),
        OptVal::Num(0),
        Locality::Local,
    ),
    def(
        Opt::Write,
        "write",
        "write",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
    def(
        Opt::WriteAny,
        "writeany",
        "wa",
        OptKind::Bool,
        OptVal::Bool(false),
        OptVal::Bool(false),
        Locality::Global,
    ),
    def(
        Opt::WriteBackup,
        "writebackup",
        "wb",
        OptKind::Bool,
        OptVal::Bool(true),
        OptVal::Bool(true),
        Locality::Global,
    ),
];

/// The table row of `opt`.
#[must_use]
pub fn opt_def(opt: Opt) -> &'static OptDef {
    OPTIONS
        .iter()
        .find(|d| d.opt == opt)
        .expect("every Opt has a row")
}

/// The option `name` (long or short) names, if ruse honours it.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static OptDef> {
    OPTIONS.iter().find(|d| d.name == name || d.short == name)
}
