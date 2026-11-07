//! The `:set` / `:setlocal` / `:setglobal` argument grammar (#614), read the way nvim v0.12.6's
//! `do_set` / `do_one_set_option` read it, and the table of the options ruse honours.
//!
//! One command line holds any number of arguments, separated by blanks (a `\` escapes a blank inside a
//! string value). Each argument is one of
//!
//! | form                     | meaning                                                      |
//! |--------------------------|--------------------------------------------------------------|
//! | `opt`                    | boolean: switch on; number/string: show the value             |
//! | `noopt` / `invopt`       | boolean: switch off / invert (any other type: `E474`)         |
//! | `opt!`                   | boolean: invert (any other type: `E488`)                      |
//! | `opt?`                   | show the value                                                |
//! | `opt&` / `opt&vim`       | reset to ruse's default / to Vim's default                    |
//! | `opt<`                   | copy the global value into the local one                      |
//! | `opt=v` / `opt:v`        | assign (boolean: `E474`; not a number: `E521`)                |
//! | `opt+=v` `opt-=v` `opt^=v` | number: add, subtract, multiply; string: append, remove, prepend |
//! | `all` / `all&`           | show every option / reset every option                        |
//! | `"…`                     | a comment: the rest of the line is ignored                    |
//!
//! Parsing stops at the first malformed argument; the arguments before it still run, in order, and the
//! error shows after them (nvim stops the same way). Range checks that depend on the current value
//! (`sw-=100` is `E487`) belong to the executor, which also stops at its first error.
//!
//! `:set` writes the local and the global value, `:setlocal` the local one and `:setglobal` the global
//! one. ruse models a local value per View for the buffer-local options the core reads (`ts`, `sw`, `et`,
//! `tw`) and per buffer for the fold options; `'scrolloff'` is global-local, with a value per window
//! that `:setlocal` sets and `:set` unsets (#1151); the other options have one value, so every form
//! writes it.
//!
//! Options honoured in part (#1151; nvim v0.12.6 is the reference):
//! - `'cpoptions'` reads `aABceFs_`, nvim's default, which is how ruse behaves; another value is refused
//!   with "option not supported", since each flag would change a different command.
//! - `'verbosefile'` reads empty; a file name is refused the same way (ruse writes no message log).
//! - `'verbose'` above 0 makes `:set opt?` add the `Last set from` line, as nvim does; ruse has no other
//!   verbose messages.
//! - `'scrollback'` stays unsupported: a terminal's scrollback cannot be viewed yet, so a limit would
//!   change nothing a user sees.

use std::borrow::Cow;

use super::nvim_options::{NVIM_BOOLEAN_OPTIONS, NVIM_OPTIONS};

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

const INT_MAX: i64 = i32::MAX as i64;
const INT_MIN: i64 = i32::MIN as i64;
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
const BREAKAT: &str = super::display_opts::BREAKAT_DEFAULT;
const LISTCHARS: &str = super::display_opts::LISTCHARS_DEFAULT;

const FOLD_METHODS: &[&str] = &["manual", "expr", "marker", "syntax", "diff", "indent"];

/// `'fileformat'`'s values.
const FILE_FORMATS: &[&str] = &["unix", "dos", "mac"];

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

/// Which copy of an option a command writes and shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetScope {
    /// `:set`.
    Both,
    /// `:setlocal`.
    Local,
    /// `:setglobal`.
    Global,
}

/// A `+=` / `-=` / `^=` / `=` assignment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    /// `^=`: multiply a number, prepend to a string.
    Prepend,
}

/// An assigned value, as typed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RawVal {
    Num(i64),
    Str(String),
}

/// One parsed argument.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SetArg {
    /// `opt?`, or a bare number/string option.
    Show(Opt),
    /// `opt` / `noopt` on a boolean.
    Bool(Opt, bool),
    /// `invopt` / `opt!`.
    Toggle(Opt),
    /// `opt&` (`vim: false`) / `opt&vim` (`vim: true`).
    Default { opt: Opt, vim: bool },
    /// `opt<`.
    FromGlobal(Opt),
    /// `opt=v`, `opt+=v`, …
    Assign { opt: Opt, op: AssignOp, val: RawVal },
    /// `all`.
    ShowAll,
    /// `all&`.
    ResetAll,
    /// An option nvim knows and ruse does not implement: skipped with a notice.
    Unsupported(String),
    /// A terminal key code (`t_xx`, `<F1>`): nvim accepts and ignores it.
    Ignored,
}

/// A parsed `:set` line: the arguments in order, each with its text (for an error message the executor
/// raises), and the error that ended parsing early, if any. No arguments and no error is a bare `:set`
/// (show the options that differ from their default).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SetLine {
    pub scope: SetScope,
    pub args: Vec<(SetArg, String)>,
    pub error: Option<String>,
}

/// Parse `:set …` / `:setlocal …` / `:setglobal …` (the command word already resolved), or `None` for
/// any other command.
#[must_use]
pub fn parse_set_command(line: &str) -> Option<SetLine> {
    let (word, rest) = line
        .find(|c: char| !c.is_ascii_alphabetic())
        .map_or((line, ""), |i| line.split_at(i));
    let scope = match word {
        "set" => SetScope::Both,
        "setlocal" => SetScope::Local,
        "setglobal" => SetScope::Global,
        _ => return None,
    };
    // `:set!` lists termcap options too in Vim; ruse has none, so it is `:set`.
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    if !(rest.is_empty() || rest.starts_with([' ', '\t', '"'])) {
        return None;
    }
    Some(parse_args(scope, rest))
}

/// Parse the arguments of a `:set` line.
#[must_use]
pub fn parse_args(scope: SetScope, args: &str) -> SetLine {
    let mut out = SetLine {
        scope,
        args: Vec::new(),
        error: None,
    };
    let b = args.as_bytes();
    let mut i = skip_white(b, 0);
    while i < b.len() {
        if b[i] == b'"' {
            break;
        }
        match parse_one(args, i) {
            Ok((arg, end, comment)) => {
                let next = skip_white(b, end);
                out.args.push((arg, args[i..next].to_string()));
                if comment {
                    break;
                }
                i = next;
            }
            Err((msg, at)) => {
                let next = skip_white(b, skip_to_white(b, at));
                out.error = Some(format!("{msg}: {}", &args[i..next]));
                break;
            }
        }
    }
    out
}

const E474: &str = "E474: Invalid argument";
const E488: &str = "E488: Trailing characters";
const E518: &str = "E518: Unknown option";
const E521: &str = "E521: Number required after =";

/// One argument starting at `start`: the argument, where it ends, and whether a `"` comment follows it
/// directly. An error carries its message and the position its text runs from (see [`parse_args`]).
fn parse_one(s: &str, start: usize) -> Result<(SetArg, usize, bool), (&'static str, usize)> {
    let b = s.as_bytes();
    let rest = &s[start..];
    let ends = |at: usize| at >= b.len() || matches!(b[at], b' ' | b'\t');
    let ends_or_comment = |at: usize| ends(at) || b[at] == b'"';
    if rest.starts_with("all") && ends_or_comment(start + 3) {
        return Ok((SetArg::ShowAll, start + 3, false));
    }
    if rest.starts_with("all&") {
        return Ok((SetArg::ResetAll, start + 4, false));
    }
    let (prefix, mut i) = if rest.starts_with("no") {
        (Prefix::No, start + 2)
    } else if rest.starts_with("inv") {
        (Prefix::Inv, start + 3)
    } else {
        (Prefix::None, start)
    };
    // A key code (`<F1>`) or a termcap option (`t_xx`): nvim has none and ignores them.
    if b.get(i) == Some(&b'<') || s[i..].starts_with("t_") {
        return Ok((SetArg::Ignored, skip_to_white(b, i), false));
    }
    let name_start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    let name = &s[name_start..i];
    let known = |n: &str| NVIM_OPTIONS.split(' ').any(|o| o == n);
    if name.is_empty() || !known(name) {
        return Err((E518, start));
    }
    let def = lookup(name);
    let is_bool = def.map_or_else(
        || NVIM_BOOLEAN_OPTIONS.split(' ').any(|o| o == name),
        |d| d.kind == OptKind::Bool,
    );
    if prefix != Prefix::None && !is_bool {
        return Err((E474, start));
    }
    let name_end = i;
    // White space may come before the operator (`:set ts =4`, `:set ai  ?`).
    let op_at = skip_white(b, name_end);
    let op = b.get(op_at).copied();
    let Some(def) = def else {
        // A boolean takes no value (`E474`), whether or not ruse implements it.
        if is_bool && matches!(op, Some(b'=' | b':')) {
            return Err((E474, start));
        }
        return Ok((
            SetArg::Unsupported(name.to_string()),
            skip_to_white(b, i),
            false,
        ));
    };
    let opt = def.opt;
    let assign = match op {
        Some(b'=' | b':') => Some((AssignOp::Set, op_at + 1)),
        Some(c @ (b'+' | b'-' | b'^')) if b.get(op_at + 1) == Some(&b'=') => Some((
            match c {
                b'+' => AssignOp::Add,
                b'-' => AssignOp::Sub,
                _ => AssignOp::Prepend,
            },
            op_at + 2,
        )),
        _ => None,
    };
    if let Some((op, value_at)) = assign {
        if is_bool {
            return Err((E474, start));
        }
        return match def.kind {
            OptKind::Num { .. } => {
                let (n, end) = parse_number(b, value_at).ok_or((E521, value_at))?;
                if !ends_or_comment(end) {
                    return Err((E521, value_at));
                }
                let comment = end < b.len() && b[end] == b'"';
                Ok((
                    SetArg::Assign {
                        opt,
                        op,
                        val: RawVal::Num(n),
                    },
                    end,
                    comment,
                ))
            }
            _ => {
                let (v, end) = string_value(s, value_at);
                Ok((
                    SetArg::Assign {
                        opt,
                        op,
                        val: RawVal::Str(v),
                    },
                    end,
                    false,
                ))
            }
        };
    }
    match op {
        Some(b'?') => {
            if !ends_or_comment(op_at + 1) {
                return Err((E488, start));
            }
            Ok((SetArg::Show(opt), op_at + 1, comment_at(b, op_at + 1)))
        }
        Some(b'!') => {
            if !is_bool || !ends_or_comment(op_at + 1) {
                return Err((E488, start));
            }
            Ok((SetArg::Toggle(opt), op_at + 1, comment_at(b, op_at + 1)))
        }
        Some(b'&') => {
            let mut end = op_at + 1;
            let vim = if s[end..].starts_with("vim") {
                end += 3;
                true
            } else if s[end..].starts_with("vi") {
                end += 2;
                true
            } else {
                false
            };
            if !ends_or_comment(end) {
                return Err((E488, start));
            }
            Ok((SetArg::Default { opt, vim }, end, comment_at(b, end)))
        }
        Some(b'<') => {
            if !ends_or_comment(op_at + 1) {
                return Err((E488, start));
            }
            Ok((SetArg::FromGlobal(opt), op_at + 1, comment_at(b, op_at + 1)))
        }
        _ => {
            // No operator. Right after the name only a blank, the end or a comment may follow.
            if !ends_or_comment(name_end) {
                return Err((E488, start));
            }
            let arg = match (is_bool, prefix) {
                (true, Prefix::None) => SetArg::Bool(opt, true),
                (true, Prefix::No) => SetArg::Bool(opt, false),
                (true, Prefix::Inv) => SetArg::Toggle(opt),
                (false, _) => SetArg::Show(opt),
            };
            Ok((arg, name_end, comment_at(b, name_end)))
        }
    }
}

fn comment_at(b: &[u8], at: usize) -> bool {
    b.get(at) == Some(&b'"')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prefix {
    None,
    No,
    Inv,
}

fn skip_white(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// The end of the argument at `i`: the next blank a `\` does not escape (nvim's `skiptowhite_esc`).
fn skip_to_white(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && !matches!(b[i], b' ' | b'\t') {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
        }
        i += 1;
    }
    i
}

/// A number the way `:set` reads one (nvim's `vim_str2nr` with every base): an optional `-`, then decimal,
/// `0x` hex, `0b` binary, `0o` or a leading-`0` octal. Saturates at `i64` (the executor rejects anything
/// past `i32` with `E474`). Returns the value and where it ends, or `None` with no digits.
fn parse_number(b: &[u8], at: usize) -> Option<(i64, usize)> {
    let mut i = at;
    let neg = b.get(i) == Some(&b'-');
    if neg {
        i += 1;
    }
    let lower = |k: usize| b.get(k).map(u8::to_ascii_lowercase);
    let (radix, mut j) = match (b.get(i), lower(i + 1)) {
        (Some(b'0'), Some(b'x')) if b.get(i + 2).is_some_and(u8::is_ascii_hexdigit) => (16, i + 2),
        (Some(b'0'), Some(b'b')) if matches!(b.get(i + 2), Some(b'0' | b'1')) => (2, i + 2),
        (Some(b'0'), Some(b'o')) if matches!(b.get(i + 2), Some(b'0'..=b'7')) => (8, i + 2),
        (Some(b'0'), Some(b'0'..=b'7')) => {
            // A leading 0 is octal only when every digit is one (`019` is decimal).
            let run = b[i..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .collect::<Vec<_>>();
            if run.iter().all(|c| (b'0'..=b'7').contains(c)) {
                (8, i + 1)
            } else {
                (10, i)
            }
        }
        _ => (10, i),
    };
    let digits_start = j;
    let mut n: i64 = 0;
    while let Some(d) = b.get(j).and_then(|&c| (c as char).to_digit(radix)) {
        n = n
            .saturating_mul(i64::from(radix))
            .saturating_add(i64::from(d));
        j += 1;
    }
    if j == digits_start {
        return None;
    }
    Some((if neg { -n } else { n }, j))
}

/// A string value from `at` to the next unescaped blank, with each `\` escape resolved.
fn string_value(s: &str, at: usize) -> (String, usize) {
    let mut out = String::new();
    let mut chars = s[at..].char_indices();
    while let Some((k, c)) = chars.next() {
        match c {
            ' ' | '\t' => return (out, at + k),
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    out.push(n);
                }
            }
            c => out.push(c),
        }
    }
    (out, s.len())
}

/// Why a value cannot be assigned to an option (the message prefix; the argument text follows it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValueError {
    /// `E487: Argument must be positive`.
    Positive,
    /// `E474: Invalid argument`.
    Invalid,
    /// A string option's own check failed (`E539: Illegal character <x>`, `E537`, …).
    Message(String),
}

impl ValueError {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            ValueError::Positive => "E487: Argument must be positive",
            ValueError::Invalid => E474,
            ValueError::Message(m) => m,
        }
    }
}

/// The value `op` makes of `current` with `val`, checked against the option's rules.
///
/// # Errors
/// The [`ValueError`] nvim raises for it.
pub fn assign_value(
    def: &OptDef,
    current: OptVal,
    op: AssignOp,
    val: &RawVal,
) -> Result<OptVal, ValueError> {
    match (def.kind, current, val) {
        (OptKind::Num { min, max }, OptVal::Num(cur), RawVal::Num(n)) => {
            let v = match op {
                AssignOp::Set => Some(*n),
                AssignOp::Add => cur.checked_add(*n),
                AssignOp::Sub => cur.checked_sub(*n),
                AssignOp::Prepend => cur.checked_mul(*n),
            }
            .ok_or(ValueError::Invalid)?;
            if !(INT_MIN..=INT_MAX).contains(&v) {
                return Err(ValueError::Invalid);
            }
            // 'scrolljump' below -100, and a negative 'scroll', are nvim's E49, not E487.
            if matches!(def.opt, Opt::ScrollJump | Opt::Scroll) && v < min {
                return Err(ValueError::Message("E49: Invalid scroll size".into()));
            }
            // Below the minimum is "not positive" (`'tabstop'` 0 too, as nvim says).
            if v < min {
                return Err(ValueError::Positive);
            }
            if v > max {
                return Err(ValueError::Invalid);
            }
            Ok(OptVal::Num(v))
        }
        (OptKind::Str { values, .. }, OptVal::Str(cur), RawVal::Str(v)) => {
            let new = match op {
                AssignOp::Set => v.clone(),
                AssignOp::Add => format!("{cur}{v}"),
                AssignOp::Prepend => format!("{v}{cur}"),
                AssignOp::Sub => cur.replacen(v.as_str(), "", 1),
            };
            values
                .iter()
                .find(|&&x| x == new)
                .map(|&x| OptVal::Str(x))
                .ok_or(ValueError::Invalid)
        }
        (OptKind::Text { list, check }, OptVal::Text(cur), RawVal::Str(v)) => {
            let new = combine(list, &cur, op, v);
            let checked = match check {
                TextCheck::None => Ok(()),
                TextCheck::FormatOptions => ruse_core::editor::check_formatoptions(&new),
                TextCheck::Comments => ruse_core::editor::check_comments(&new),
                TextCheck::CommentString => {
                    if new.is_empty() || new.contains("%s") {
                        Ok(())
                    } else {
                        Err(ruse_core::editor::OptionValueError::NoPercentS)
                    }
                }
                TextCheck::TabStops => {
                    return ruse_core::VarStops::parse(&new)
                        .map(|_| OptVal::Text(Cow::Owned(new)))
                        .ok_or(ValueError::Invalid);
                }
                TextCheck::BackupCopy | TextCheck::BackupExt | TextCheck::FileFormats => {
                    let ok = match check {
                        TextCheck::BackupCopy => backupcopy_valid(&new),
                        TextCheck::FileFormats => new
                            .split(',')
                            .all(|f| f.is_empty() || FILE_FORMATS.contains(&f)),
                        _ => !new.contains('/'),
                    };
                    return if ok {
                        Ok(OptVal::Text(Cow::Owned(new)))
                    } else {
                        Err(ValueError::Invalid)
                    };
                }
                TextCheck::FoldOpen => {
                    return if foldopen_valid(&new) {
                        Ok(OptVal::Text(Cow::Owned(new)))
                    } else {
                        Err(ValueError::Invalid)
                    };
                }
                TextCheck::MessagesOpt => {
                    return messagesopt_history(&new)
                        .map(|_| OptVal::Text(Cow::Owned(new)))
                        .ok_or(ValueError::Invalid);
                }
                TextCheck::ListChars
                | TextCheck::BreakIndentOpt
                | TextCheck::ShowBreak
                | TextCheck::Display
                | TextCheck::SignColumn
                | TextCheck::FillChars
                | TextCheck::CursorLineOpt
                | TextCheck::ColorColumn
                | TextCheck::FoldColumn => {
                    display_check(check, &new).map_err(|e| ValueError::Message(e.to_string()))?;
                    Ok(())
                }
            };
            checked.map_err(|e| ValueError::Message(e.to_string()))?;
            Ok(OptVal::Text(Cow::Owned(new)))
        }
        _ => Err(ValueError::Invalid),
    }
}

/// The display options' value checks (#615, #616), as nvim's `did_set_*` callbacks make them.
fn display_check(check: TextCheck, v: &str) -> Result<(), super::display_opts::DisplayOptError> {
    use super::display_opts as d;
    match check {
        TextCheck::ListChars => d::parse_listchars(v).map(|_| ()),
        TextCheck::BreakIndentOpt => d::parse_briopt(v).map(|_| ()),
        TextCheck::ShowBreak => d::check_showbreak(v),
        TextCheck::Display => d::check_display(v),
        TextCheck::SignColumn => d::parse_signcolumn(v).map(|_| ()),
        TextCheck::FillChars => d::parse_fillchars(v).map(|_| ()),
        TextCheck::CursorLineOpt => d::parse_culopt(v).map(|_| ()),
        TextCheck::ColorColumn => d::parse_colorcolumn(v).map(|_| ()),
        TextCheck::FoldColumn => d::parse_foldcolumn(v).map(|_| ()),
        _ => Ok(()),
    }
}

/// `s` with each flag that appears again later dropped.
fn dedup_flags(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|&(i, c)| !chars[i + 1..].contains(c))
        .map(|(_, c)| c)
        .collect()
}

/// `cur` combined with `v` by `op` the way nvim's `do_set_option_string` combines a string option.
fn combine(list: ListKind, cur: &str, op: AssignOp, v: &str) -> String {
    match (op, list) {
        // A flag list keeps the LAST of a flag that appears twice (nvim's `stropt_remove_dupflags`).
        (AssignOp::Set, ListKind::Flags) => dedup_flags(v),
        (AssignOp::Set, _) => v.to_string(),
        (AssignOp::Add, ListKind::Flags) => dedup_flags(&format!("{cur}{v}")),
        (AssignOp::Prepend, ListKind::Flags) => dedup_flags(&format!("{v}{cur}")),
        (AssignOp::Add | AssignOp::Prepend, ListKind::Comma) => {
            if v.is_empty() || cur.split(',').any(|item| item == v) {
                return cur.to_string();
            }
            match (cur.is_empty(), op) {
                (true, _) => v.to_string(),
                (false, AssignOp::Add) => format!("{cur},{v}"),
                (false, _) => format!("{v},{cur}"),
            }
        }
        (AssignOp::Add, ListKind::Plain) => format!("{cur}{v}"),
        (AssignOp::Prepend, ListKind::Plain) => format!("{v}{cur}"),
        (AssignOp::Sub, ListKind::Comma) => {
            let items: Vec<&str> = cur.split(',').collect();
            match items.iter().position(|item| *item == v) {
                Some(i) => {
                    let mut items = items;
                    items.remove(i);
                    items.join(",")
                }
                None => cur.to_string(),
            }
        }
        (AssignOp::Sub, ListKind::Flags) => dedup_flags(&cur.replacen(v, "", 1)),
        (AssignOp::Sub, _) => cur.replacen(v, "", 1),
    }
}

/// How `:set opt?` shows a value: `  name=value` for a number or string, `  name` / `noname` for a
/// boolean (nvim's `showoneopt`).
#[must_use]
pub fn show_one(def: &OptDef, val: OptVal) -> String {
    match val {
        OptVal::Bool(true) => format!("  {}", def.name),
        OptVal::Bool(false) => format!("no{}", def.name),
        OptVal::Num(n) => format!("  {}={n}", def.name),
        OptVal::Str(s) => format!("  {}={s}", def.name),
        OptVal::Text(s) => format!("  {}={s}", def.name),
    }
}

/// The rows of an option listing (`:set`, `:set all`) on a `cols`-wide screen, the way nvim's
/// `showoptions` lays them out: a title row, then the short entries in columns 20 cells apart, filled
/// column by column, then every entry too long for a column on a row of its own.
#[must_use]
pub fn listing(title: &str, items: &[(&OptDef, OptVal)], cols: usize) -> Vec<String> {
    const INC: usize = 20;
    const GAP: usize = 3;
    let mut rows = vec![title.to_string()];
    let fits = |d: &OptDef, v: &OptVal| match v {
        OptVal::Bool(_) => true,
        _ => show_one(d, v.clone()).len() - 2 <= INC - GAP,
    };
    let short: Vec<String> = items
        .iter()
        .filter(|(d, v)| fits(d, v))
        .map(|(d, v)| show_one(d, v.clone()))
        .collect();
    if !short.is_empty() {
        let ncols = ((cols + GAP).saturating_sub(3) / INC).max(1);
        let nrows = short.len().div_ceil(ncols);
        for r in 0..nrows {
            let mut line = String::new();
            for (k, item) in short.iter().enumerate().skip(r).step_by(nrows) {
                let col = (k / nrows) * INC;
                while line.len() < col {
                    line.push(' ');
                }
                line.push_str(item);
            }
            rows.push(line);
        }
    }
    rows.extend(
        items
            .iter()
            .filter(|(d, v)| !fits(d, v))
            .map(|(d, v)| show_one(d, v.clone())),
    );
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<SetArg> {
        let l = parse_args(SetScope::Both, s);
        assert_eq!(l.error, None, "{s}");
        l.args.into_iter().map(|(a, _)| a).collect()
    }

    fn err(s: &str) -> String {
        parse_args(SetScope::Both, s).error.expect(s)
    }

    #[test]
    fn forms() {
        use SetArg::*;
        assert_eq!(args("ic"), [Bool(Opt::IgnoreCase, true)]);
        assert_eq!(args("noic"), [Bool(Opt::IgnoreCase, false)]);
        assert_eq!(args("invic"), [Toggle(Opt::IgnoreCase)]);
        assert_eq!(args("ic!"), [Toggle(Opt::IgnoreCase)]);
        assert_eq!(args("noic!"), [Toggle(Opt::IgnoreCase)]);
        assert_eq!(args("ic?"), [Show(Opt::IgnoreCase)]);
        assert_eq!(args("noic?"), [Show(Opt::IgnoreCase)]);
        assert_eq!(args("ts"), [Show(Opt::TabStop)]);
        assert_eq!(args("ts?"), [Show(Opt::TabStop)]);
        assert_eq!(args("ts ?"), [Show(Opt::TabStop)]);
        assert_eq!(
            args("ts&"),
            [Default {
                opt: Opt::TabStop,
                vim: false
            }]
        );
        assert_eq!(
            args("ts&vim"),
            [Default {
                opt: Opt::TabStop,
                vim: true
            }]
        );
        assert_eq!(args("ts<"), [FromGlobal(Opt::TabStop)]);
        let assign = |opt, op, n| Assign {
            opt,
            op,
            val: RawVal::Num(n),
        };
        assert_eq!(args("ts=4"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
        assert_eq!(args("ts:4"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
        assert_eq!(args("ts =4"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
        assert_eq!(args("sw+=2"), [assign(Opt::ShiftWidth, AssignOp::Add, 2)]);
        assert_eq!(args("sw-=2"), [assign(Opt::ShiftWidth, AssignOp::Sub, 2)]);
        assert_eq!(
            args("sw^=2"),
            [assign(Opt::ShiftWidth, AssignOp::Prepend, 2)]
        );
        assert_eq!(args("ts=0x10"), [assign(Opt::TabStop, AssignOp::Set, 16)]);
        assert_eq!(args("ts=010"), [assign(Opt::TabStop, AssignOp::Set, 8)]);
        assert_eq!(args("ts=019"), [assign(Opt::TabStop, AssignOp::Set, 19)]);
        assert_eq!(args("ts=0b101"), [assign(Opt::TabStop, AssignOp::Set, 5)]);
        assert_eq!(args("ts=0o7"), [assign(Opt::TabStop, AssignOp::Set, 7)]);
        assert_eq!(args("so=-2"), [assign(Opt::ScrollOff, AssignOp::Set, -2)]);
        assert_eq!(
            args("fdm=indent"),
            [Assign {
                opt: Opt::FoldMethod,
                op: AssignOp::Set,
                val: RawVal::Str("indent".into())
            }]
        );
        assert_eq!(
            args("ts=4 ic sw?"),
            [
                assign(Opt::TabStop, AssignOp::Set, 4),
                Bool(Opt::IgnoreCase, true),
                Show(Opt::ShiftWidth)
            ]
        );
        assert_eq!(
            args("ic  ic?"),
            [Bool(Opt::IgnoreCase, true), Show(Opt::IgnoreCase)]
        );
        assert_eq!(args("all"), [ShowAll]);
        assert_eq!(args("all&"), [ResetAll]);
        assert_eq!(args("t_xx=1 <F1>"), [Ignored, Ignored]);
        assert_eq!(args("wrap"), [Bool(Opt::Wrap, true)]);
        assert_eq!(args("spell"), [Unsupported("spell".into())]);
        // Comments.
        assert_eq!(args("\"x"), []);
        assert_eq!(args("ic\"x"), [Bool(Opt::IgnoreCase, true)]);
        assert_eq!(
            args("ts=4 \"c sw=2"),
            [assign(Opt::TabStop, AssignOp::Set, 4)]
        );
        assert_eq!(args("ts=4\"c"), [assign(Opt::TabStop, AssignOp::Set, 4)]);
    }

    /// The messages nvim v0.12.6 gives (probed headless with `execute()`), including the argument text it
    /// quotes: the argument and the blanks after it.
    #[test]
    fn errors_quote_nvim() {
        assert_eq!(err("bogus"), "E518: Unknown option: bogus");
        assert_eq!(err("ts=4 bogus sw=2"), "E518: Unknown option: bogus ");
        assert_eq!(err("bogus?"), "E518: Unknown option: bogus?");
        assert_eq!(err("nobogus"), "E518: Unknown option: nobogus");
        assert_eq!(err("no"), "E518: Unknown option: no");
        assert_eq!(err("4"), "E518: Unknown option: 4");
        assert_eq!(err("=4"), "E518: Unknown option: =4");
        assert_eq!(err("all&vim"), "E518: Unknown option: vim");
        assert_eq!(err("ts!"), "E488: Trailing characters: ts!");
        assert_eq!(err("ts! sw=3"), "E488: Trailing characters: ts! ");
        assert_eq!(err("ic?ts"), "E488: Trailing characters: ic?ts");
        assert_eq!(err("ic&x"), "E488: Trailing characters: ic&x");
        assert_eq!(err("ts&vim&"), "E488: Trailing characters: ts&vim&");
        assert_eq!(err("ts<4"), "E488: Trailing characters: ts<4");
        assert_eq!(err("ic!x"), "E488: Trailing characters: ic!x");
        assert_eq!(err("sw*=2"), "E488: Trailing characters: sw*=2");
        assert_eq!(err("ic=1"), "E474: Invalid argument: ic=1");
        assert_eq!(err("ic=1 ts=5"), "E474: Invalid argument: ic=1 ");
        assert_eq!(err("ic+=1"), "E474: Invalid argument: ic+=1");
        assert_eq!(err("noic=1"), "E474: Invalid argument: noic=1");
        assert_eq!(err("nosw"), "E474: Invalid argument: nosw");
        assert_eq!(err("invsw"), "E474: Invalid argument: invsw");
        assert_eq!(err("nowrapscan=1"), "E474: Invalid argument: nowrapscan=1");
        assert_eq!(err("ts=abc"), "E521: Number required after =: ts=abc");
        assert_eq!(err("ts= 4"), "E521: Number required after =: ts= ");
        assert_eq!(err("ts=4x"), "E521: Number required after =: ts=4x");
        assert_eq!(err("ts=4,"), "E521: Number required after =: ts=4,");
        assert_eq!(err("ts="), "E521: Number required after =: ts=");
        assert_eq!(err("ts:"), "E521: Number required after =: ts:");
        assert_eq!(err("ts=+4"), "E521: Number required after =: ts=+4");
        assert_eq!(err("ts+=abc"), "E521: Number required after =: ts+=abc");
        assert_eq!(err("ts==4"), "E521: Number required after =: ts==4");
        assert_eq!(err("ts  =  4"), "E521: Number required after =: ts  =  ");
        assert_eq!(err("ts=4\\ "), "E521: Number required after =: ts=4\\ ");
        assert_eq!(
            err("sw=3 sw=x sw=5"),
            "E521: Number required after =: sw=x "
        );
    }

    #[test]
    fn earlier_arguments_survive_an_error() {
        let l = parse_args(SetScope::Both, "sw=3 sw=x sw=5");
        assert_eq!(l.args.len(), 1);
        assert!(l.error.is_some());
    }

    #[test]
    fn values_follow_nvim_rules() {
        let ts = opt_def(Opt::TabStop);
        let sw = opt_def(Opt::ShiftWidth);
        let so = opt_def(Opt::ScrollOff);
        let num = |n| RawVal::Num(n);
        let set = AssignOp::Set;
        assert_eq!(
            assign_value(ts, OptVal::Num(8), set, &num(0)),
            Err(ValueError::Positive)
        );
        assert_eq!(
            assign_value(ts, OptVal::Num(8), set, &num(-1)),
            Err(ValueError::Positive)
        );
        assert_eq!(
            assign_value(ts, OptVal::Num(8), set, &num(10000)),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(ts, OptVal::Num(8), set, &num(9999)),
            Ok(OptVal::Num(9999))
        );
        assert_eq!(
            assign_value(ts, OptVal::Num(8), set, &num(99_999_999_999)),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(sw, OptVal::Num(8), set, &num(-1)),
            Err(ValueError::Positive)
        );
        assert_eq!(
            assign_value(sw, OptVal::Num(4), AssignOp::Sub, &num(100)),
            Err(ValueError::Positive)
        );
        assert_eq!(
            assign_value(sw, OptVal::Num(i32::MAX.into()), AssignOp::Add, &num(1)),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(sw, OptVal::Num(5), AssignOp::Prepend, &num(3)),
            Ok(OptVal::Num(15))
        );
        assert_eq!(
            assign_value(so, OptVal::Num(0), set, &num(-2)),
            Ok(OptVal::Num(-2))
        );
        let fdm = opt_def(Opt::FoldMethod);
        let s = |v: &str| RawVal::Str(v.into());
        let cur = OptVal::Str("manual");
        assert_eq!(
            assign_value(fdm, cur.clone(), set, &s("bogus")),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(fdm, cur.clone(), set, &s("")),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(fdm, cur.clone(), AssignOp::Add, &s("")),
            Ok(cur.clone())
        );
        assert_eq!(
            assign_value(fdm, cur.clone(), AssignOp::Add, &s("x")),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(fdm, cur.clone(), AssignOp::Sub, &s("al")),
            Err(ValueError::Invalid)
        );
        assert_eq!(
            assign_value(fdm, cur.clone(), set, &s("marker")),
            Ok(OptVal::Str("marker"))
        );
    }

    #[test]
    fn string_options_combine_and_check_like_nvim() {
        let set = AssignOp::Set;
        let s = |v: &str| RawVal::Str(v.into());
        let text = |v: &str| OptVal::Text(Cow::Owned(v.into()));
        let fo = opt_def(Opt::FormatOptions);
        // nvim v0.12.6: a flag list drops the flags already there, unless the value is already in it.
        assert_eq!(
            assign_value(fo, text("tcqj"), AssignOp::Add, &s("jr")),
            Ok(text("tcqjr"))
        );
        assert_eq!(
            assign_value(fo, text("tcqj"), AssignOp::Add, &s("qj")),
            Ok(text("tcqj"))
        );
        assert_eq!(
            assign_value(fo, text("tcqj"), AssignOp::Add, &s("cq")),
            Ok(text("tjcq"))
        );
        assert_eq!(
            assign_value(fo, text("tcqj"), AssignOp::Sub, &s("cq")),
            Ok(text("tj"))
        );
        assert_eq!(
            assign_value(fo, text("tcqj"), AssignOp::Sub, &s("qc")),
            Ok(text("tcqj"))
        );
        assert_eq!(
            assign_value(fo, text("tcqj"), AssignOp::Prepend, &s("r")),
            Ok(text("rtcqj"))
        );
        assert_eq!(
            assign_value(fo, text("tcqj"), set, &s("xyz")),
            Err(ValueError::Message("E539: Illegal character <x>".into()))
        );
        let com = opt_def(Opt::Comments);
        assert_eq!(
            assign_value(com, text("b:x"), AssignOp::Add, &s(":;")),
            Ok(text("b:x,:;"))
        );
        assert_eq!(
            assign_value(com, text("b:x,:;"), AssignOp::Add, &s(":;")),
            Ok(text("b:x,:;"))
        );
        assert_eq!(
            assign_value(com, text("b:x,:;"), AssignOp::Sub, &s("b:x")),
            Ok(text(":;"))
        );
        assert_eq!(
            assign_value(com, text("b:x"), AssignOp::Prepend, &s(":;")),
            Ok(text(":;,b:x"))
        );
        assert_eq!(
            assign_value(com, text(""), set, &s(":,")),
            Err(ValueError::Message("E525: Zero length string".into()))
        );
        let cms = opt_def(Opt::CommentString);
        assert_eq!(assign_value(cms, text(""), set, &s("#%s")), Ok(text("#%s")));
        assert!(matches!(
            assign_value(cms, text(""), AssignOp::Add, &s("x")),
            Err(ValueError::Message(m)) if m.starts_with("E537")
        ));
        let fp = opt_def(Opt::FormatPrg);
        assert_eq!(
            assign_value(fp, text(""), AssignOp::Add, &s(" -w")),
            Ok(text(" -w"))
        );
    }

    #[test]
    fn show_and_listing_match_nvim() {
        assert_eq!(
            show_one(opt_def(Opt::TabStop), OptVal::Num(8)),
            "  tabstop=8"
        );
        assert_eq!(
            show_one(opt_def(Opt::IgnoreCase), OptVal::Bool(true)),
            "  ignorecase"
        );
        assert_eq!(
            show_one(opt_def(Opt::IgnoreCase), OptVal::Bool(false)),
            "noignorecase"
        );
        // nvim's `:set` with ic, so=100, ts=4, sw=4, tw=2147483647 (and its own `scroll=11`) on 80 columns:
        // "  ignorecase          scrolloff=100       tabstop=4" / "  scroll=11           shiftwidth=4" /
        // "  textwidth=2147483647": short entries fill 20-cell columns top to bottom, a long one gets its
        // own row. Without `scroll` the four short ones fit one row.
        let mut items = vec![
            (opt_def(Opt::IgnoreCase), OptVal::Bool(true)),
            (opt_def(Opt::ScrollOff), OptVal::Num(100)),
            (opt_def(Opt::ShiftWidth), OptVal::Num(4)),
            (opt_def(Opt::TabStop), OptVal::Num(4)),
            (opt_def(Opt::TextWidth), OptVal::Num(2_147_483_647)),
        ];
        assert_eq!(
            listing("--- Options ---", &items, 80),
            [
                "--- Options ---",
                "  ignorecase          scrolloff=100       shiftwidth=4        tabstop=4",
                "  textwidth=2147483647",
            ]
        );
        items.insert(2, (opt_def(Opt::FoldLevel), OptVal::Num(1)));
        assert_eq!(
            listing("--- Options ---", &items, 80)[1..3],
            [
                "  ignorecase          foldlevel=1         tabstop=4",
                "  scrolloff=100       shiftwidth=4",
            ]
        );
    }

    #[test]
    fn command_words() {
        assert_eq!(
            parse_set_command("set").map(|l| l.scope),
            Some(SetScope::Both)
        );
        assert_eq!(
            parse_set_command("setlocal ts=2").map(|l| l.scope),
            Some(SetScope::Local)
        );
        assert_eq!(
            parse_set_command("setglobal").map(|l| l.scope),
            Some(SetScope::Global)
        );
        assert_eq!(parse_set_command("setx"), None);
        assert_eq!(parse_set_command("sort"), None);
    }
}
