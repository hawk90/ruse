//! The key schema: every core key's type, scope, merge strategy and lock, mirroring
//! `spec/config-schema.yaml` (a test keeps them equal).

/// A config layer's class, lowest precedence first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LayerKind {
    Machine,
    User,
    Workspace,
}

impl LayerKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            LayerKind::Machine => "machine",
            LayerKind::User => "user",
            LayerKind::Workspace => "workspace",
        }
    }
}

/// The broadest layer allowed to write a key (`scope` in the schema).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Machine,
    User,
    Workspace,
}

impl Scope {
    pub(super) fn allows(self, layer: LayerKind) -> bool {
        match self {
            Scope::Machine => layer == LayerKind::Machine,
            Scope::User => layer != LayerKind::Workspace,
            Scope::Workspace => true,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Scope::Machine => "machine",
            Scope::User => "user",
            Scope::Workspace => "workspace",
        }
    }
}

/// How layers fold a key (`merge` in the schema).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Merge {
    Replace,
    Append,
    SetUnion,
    /// `deep-merge`.
    Deep,
}

impl Merge {
    /// The schema's spelling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Merge::Replace => "replace",
            Merge::Append => "append",
            Merge::SetUnion => "set-union",
            Merge::Deep => "deep-merge",
        }
    }
}

/// A key's type (`type`, with `enum` / `min` / `max`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ty {
    Bool,
    Int { min: i64, max: i64 },
    Enum(&'static [&'static str]),
    Str,
    List,
    Map,
}

/// One schema key.
#[derive(Clone, Copy, Debug)]
pub struct KeyDef {
    pub key: &'static str,
    pub ty: Ty,
    pub scope: Scope,
    pub merge: Merge,
    pub locked: bool,
    /// The default of a list key, which the layers' `append` / `set-union` fold onto.
    pub list_default: &'static [&'static str],
}

const fn key(key: &'static str, ty: Ty, scope: Scope, merge: Merge, locked: bool) -> KeyDef {
    KeyDef {
        key,
        ty,
        scope,
        merge,
        locked,
        list_default: &[],
    }
}

const fn keymap(name: &'static str) -> KeyDef {
    key(name, Ty::Map, Scope::User, Merge::Deep, false)
}

const fn int(min: i64, max: i64) -> Ty {
    Ty::Int { min, max }
}

/// Every core key, as `spec/config-schema.yaml` declares it.
pub const SCHEMA: &[KeyDef] = &[
    key(
        "input.profile",
        Ty::Enum(&["vim", "emacs", "native"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    keymap("keymap.normal"),
    keymap("keymap.operator_pending"),
    keymap("keymap.insert"),
    keymap("keymap.cmdline"),
    keymap("keymap.visual"),
    keymap("keymap.select"),
    keymap("keymap.terminal"),
    keymap("keymap.lang"),
    key(
        "editor.tab_width",
        int(1, 16),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.shift_width",
        int(0, 16),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.indent_style",
        Ty::Enum(&["space", "tab"]),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.softtabstop",
        int(-1, 16),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.autoindent",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.smartindent",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.copyindent",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.preserveindent",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.smarttab",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.shiftround",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.line_endings",
        Ty::Enum(&["lf", "crlf", "auto"]),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.scrolloff",
        int(0, 99),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.foldopen",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.sidescroll",
        int(0, 999),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.sidescrolloff",
        int(0, 999),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.textwidth",
        int(0, 999),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.wrapmargin",
        int(0, 999),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.formatoptions",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.comments",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.commentstring",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.formatlistpat",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.formatprg",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        true,
    ),
    key(
        "editor.equalprg",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        true,
    ),
    key(
        "editor.cinwords",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.lisp",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.lispwords",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.vartabstop",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.varsofttabstop",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.cindent",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.cinoptions",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.cinkeys",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.cinscopedecls",
        Ty::Str,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.joinspaces",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.ignorecase",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.smartcase",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.wrapscan",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.hlsearch",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.incsearch",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.tildeop",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.fixeol",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.hidden",
        Ty::Bool,
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    key(
        "editor.confirm",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.autowrite",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.autowriteall",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "editor.autoread",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.conceal",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key("render.wrap", Ty::Bool, Scope::User, Merge::Replace, false),
    key(
        "render.linebreak",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.breakindent",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.showbreak",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.number",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.relativenumber",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.numberwidth",
        int(1, 20),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.signcolumn",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key("render.list", Ty::Bool, Scope::User, Merge::Replace, false),
    key(
        "render.listchars",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.cursorline",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.cursorlineopt",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.cursorcolumn",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.colorcolumn",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.fillchars",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.foldcolumn",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "render.smoothscroll",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.watcher",
        Ty::Enum(&["native", "poll", "off"]),
        Scope::Workspace,
        Merge::Replace,
        false,
    ),
    KeyDef {
        list_default: &[".git", "node_modules", "target"],
        ..key(
            "files.exclude",
            Ty::List,
            Scope::Workspace,
            Merge::Append,
            false,
        )
    },
    key("files.backup", Ty::Bool, Scope::User, Merge::Replace, false),
    key(
        "files.writebackup",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.backupdir",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.backupext",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.backupcopy",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.backupskip",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.patchmode",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key("files.write", Ty::Bool, Scope::User, Merge::Replace, false),
    key(
        "files.writeany",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key("files.fsync", Ty::Bool, Scope::User, Merge::Replace, false),
    key(
        "emacs.make_backup_files",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.backup_directory_alist",
        Ty::Map,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.version_control",
        Ty::Enum(&["nil", "t", "never"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.kept_new_versions",
        int(0, 9999),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.kept_old_versions",
        int(0, 9999),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.delete_old_versions",
        Ty::Enum(&["nil", "t", "other"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.backup_by_copying",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.backup_by_copying_when_linked",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.backup_by_copying_when_mismatch",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.backup_by_copying_when_privileged_mismatch",
        int(-1, 2147483647),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.file_precious_flag",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.break_hardlink_on_save",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.file_preserve_symlinks_on_save",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.require_final_newline",
        Ty::Enum(&["t", "nil", "visit", "visit-save", "ask"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.mode_require_final_newline",
        Ty::Enum(&["t", "nil", "visit", "visit-save", "ask"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.temporary_file_directory",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.small_temporary_file_directory",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.view_read_only",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.confirm_kill_emacs",
        Ty::Enum(&["nil", "yes-or-no-p", "y-or-n-p"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.confirm_kill_processes",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.buffer_offer_save",
        Ty::Enum(&["nil", "t", "always"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.query_about_changed_file",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.revert_without_query",
        Ty::List,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.revert_buffer_quick_short_answers",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.save_some_buffers_default_predicate",
        Ty::Enum(&["nil", "save-some-buffers-root"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.fileformats",
        Ty::Str,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.large_file_warning_threshold",
        int(-1, 9223372036854775807),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.out_of_memory_warning_percentage",
        int(-1, 100),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.swapfile",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "files.updatecount",
        int(0, 2147483647),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.auto_save_default",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.auto_save_visited_mode",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.auto_save_visited_interval",
        int(0, 2147483647),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.find_file_wildcards",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.find_file_visit_truename",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.find_file_suppress_same_file_warnings",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.confirm_nonexistent_file_or_buffer",
        Ty::Enum(&["after-completion", "nil", "t"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.find_sibling_rules",
        Ty::Map,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.directory_abbrev_alist",
        Ty::Map,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.auto_mode_case_fold",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "emacs.change_major_mode_with_file_name",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
    key("ui.theme", Ty::Str, Scope::User, Merge::Replace, false),
    key(
        "update.channel",
        Ty::Enum(&["stable", "beta", "nightly"]),
        Scope::User,
        Merge::Replace,
        false,
    ),
    key(
        "remote.trust_prompt",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        true,
    ),
    key(
        "workspace.trusted_roots",
        Ty::List,
        Scope::User,
        Merge::SetUnion,
        true,
    ),
    key(
        "plugins.allow_unsigned",
        Ty::Bool,
        Scope::Machine,
        Merge::Replace,
        true,
    ),
    key(
        "telemetry.enabled",
        Ty::Bool,
        Scope::User,
        Merge::Replace,
        false,
    ),
];

/// The schema row of `name`.
#[must_use]
pub fn key_def(name: &str) -> Option<&'static KeyDef> {
    SCHEMA.iter().find(|k| k.key == name)
}
