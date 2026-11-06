//! The config loader (C-CONFIG, #614; `docs/design/config-model.md`). Three file layers stack over the
//! schema defaults, lowest first: machine (`/etc/ruse/config.toml`), user
//! (`${XDG_CONFIG_HOME:-~/.config}/ruse/config.toml`) and workspace (`.ruse/config.toml` in the
//! workspace root). Each key's `scope`, `merge` and `security_locked` come from [`SCHEMA`], which mirrors
//! `spec/config-schema.yaml` (a test keeps them equal):
//!
//! - a layer broader than a key's `scope` is inert for it, and a workspace never sets a
//!   `security_locked` key; such an entry is dropped with `config.locked_key_ignored` (CFG-LOCK-001);
//! - `replace` keys take the highest layer's value, except that a machine value pins a locked or
//!   `scope: machine` key; `append` lists concatenate, `set-union` lists de-duplicate and sort, and
//!   `deep-merge` maps merge per key; a `!item` element removes an earlier one (CFG-MERGE-001);
//! - one bad key never prevents startup (CFG-SAFE-001): an unknown key warns (`config.unknown_key`), a
//!   value of the wrong type or out of range falls back to the layer below (`config.invalid_value`), and a
//!   file that does not parse is quarantined whole (`config.layer_quarantined`);
//! - the workspace layer is merged only while the workspace is trusted and the trust record's content
//!   hash still matches `.ruse/` (D-062, [`crate::trust`]); otherwise it is `untrusted` or `stale`, and
//!   inert. Keymaps are `scope: user`, so a workspace never binds keys;
//! - a key under a name an older schema version used is migrated ([`MIGRATIONS`], CFG-MIG-001): a rename
//!   loads under the new name, a removed key stays inert with a warning; the file is never rewritten.
//!
//! Every effective value carries its source (layer, file, line) for `:verbose set` and `:inspect config`.
//!
//! Safe mode (CFG-SAFE-001, [`SafeMode`]) loads the machine layer alone: `ruse --safe-mode`, or after
//! [`boot::CRASH_LIMIT`] starts in a row died between loading the config and their first frame. A
//! session reloads the config when a layer file changes ([`Stamp`], [`changed_keys`]).

pub mod boot;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

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
    fn allows(self, layer: LayerKind) -> bool {
        match self {
            Scope::Machine => layer == LayerKind::Machine,
            Scope::User => layer != LayerKind::Workspace,
            Scope::Workspace => true,
        }
    }

    fn label(self) -> &'static str {
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

/// A config value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<String>),
    Map(BTreeMap<String, Value>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::List(l) => write!(f, "{l:?}"),
            Value::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// Where a value came from: its layer, file and 1-based line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Source {
    pub layer: LayerKind,
    pub path: PathBuf,
    pub line: usize,
}

/// A typed diagnostic (`config-model.md` Observability), shown once at startup.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// `config.unknown_key`, `config.invalid_value`, `config.layer_quarantined`, `config.locked_key_ignored`
    /// `config.workspace_inert` or `config.migrated`.
    pub code: &'static str,
    pub message: String,
}

/// What became of one layer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LayerState {
    /// No file.
    Absent,
    /// Parsed and merged.
    Loaded,
    /// The file could not be read or parsed: inert.
    Quarantined(String),
    /// A workspace layer with no trust record: inert until `:trust`.
    Untrusted,
    /// A workspace layer whose `.ruse/` changed since `:trust` recorded it: inert until `:trust` again.
    Stale,
    /// A user or workspace layer in safe mode: not read.
    SafeMode,
}

impl LayerState {
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            LayerState::Absent => "absent",
            LayerState::Loaded => "loaded",
            LayerState::Quarantined(_) => "quarantined",
            LayerState::Untrusted => "untrusted",
            LayerState::Stale => "stale",
            LayerState::SafeMode => "safe-mode",
        }
    }
}

/// One layer's report for `:inspect config`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LayerReport {
    pub kind: LayerKind,
    pub path: PathBuf,
    pub state: LayerState,
}

/// One effective value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Effective {
    pub value: Value,
    /// The highest layer that contributed.
    pub source: Source,
}

/// The loaded configuration: every key some layer set, the layers, and the diagnostics.
#[derive(Clone, Default, Debug)]
pub struct Config {
    pub values: BTreeMap<&'static str, Effective>,
    pub layers: Vec<LayerReport>,
    pub diagnostics: Vec<Diagnostic>,
    /// Every entry each layer contributed, by key (for `:inspect config {key}`).
    pub contributions: BTreeMap<&'static str, Vec<(Value, Source)>>,
    /// Why the user and workspace layers were not read, in safe mode.
    pub safe_mode: Option<SafeMode>,
}

/// Why a session runs in safe mode (CFG-SAFE-001): only the schema defaults and the machine layer apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SafeMode {
    /// `ruse --safe-mode`.
    Asked,
    /// This many starts in a row loaded a user or workspace layer and ended before their first frame.
    CrashLoop { failed: u32 },
}

impl SafeMode {
    /// Why, as `:inspect config` and the startup message say it.
    #[must_use]
    pub fn reason(self) -> String {
        match self {
            SafeMode::Asked => "--safe-mode".to_string(),
            SafeMode::CrashLoop { failed } => {
                format!("after {failed} crashed starts")
            }
        }
    }
}

impl Config {
    /// The effective value of `key`, if a layer set it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Effective> {
        self.values.get(key)
    }
}

/// Where the layers and the trust records live.
#[derive(Clone, Debug, Default)]
pub struct ConfigPaths {
    pub machine: Option<PathBuf>,
    pub user: Option<PathBuf>,
    /// The workspace root (the working directory); its layer is `<root>/.ruse/config.toml`.
    pub workspace_root: Option<PathBuf>,
    /// The user-side trust record directory (`${XDG_STATE_HOME:-~/.local/state}/ruse/trust`).
    pub trust_dir: Option<PathBuf>,
}

impl ConfigPaths {
    /// The paths for a session started in `cwd`, from the environment: `XDG_CONFIG_HOME` / `HOME` for
    /// the user layer, `XDG_STATE_HOME` / `HOME` for the trust records. There is no environment override
    /// of the machine layer: it is the administrator's, and pins keys above the user.
    #[must_use]
    pub fn from_env(cwd: &Path) -> ConfigPaths {
        let var = |n: &str| {
            std::env::var_os(n)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME");
        let config_home =
            var("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
        ConfigPaths {
            machine: Some(PathBuf::from("/etc/ruse/config.toml")),
            user: config_home.map(|c| c.join("ruse").join("config.toml")),
            workspace_root: Some(cwd.to_path_buf()),
            trust_dir: crate::trust::trust_dir(),
        }
    }

    /// The workspace layer's file.
    #[must_use]
    pub fn workspace_file(&self) -> Option<PathBuf> {
        self.workspace_root
            .as_ref()
            .map(|r| r.join(".ruse").join("config.toml"))
    }
}

/// One parsed entry of a layer.
#[derive(Clone, Debug)]
struct Entry {
    def: &'static KeyDef,
    value: Value,
    line: usize,
}

/// The largest config file read (a config file is a few kilobytes; this bounds a hostile one).
const MAX_FILE: u64 = 1 << 20;

/// Load every layer at `paths`, or in safe mode (`safe`) the machine layer alone: the user and workspace
/// layers are reported as `safe-mode` and not read. Never fails: problems become diagnostics and inert
/// layers.
#[must_use]
pub fn load_mode(paths: &ConfigPaths, safe: Option<SafeMode>) -> Config {
    let mut cfg = Config {
        safe_mode: safe,
        ..Config::default()
    };
    let mut layers: Vec<(LayerKind, PathBuf, Vec<Entry>)> = Vec::new();
    for (kind, path) in [
        (LayerKind::Machine, paths.machine.clone()),
        (LayerKind::User, paths.user.clone()),
    ] {
        let Some(path) = path else { continue };
        if safe.is_some() && kind == LayerKind::User {
            cfg.layers.push(LayerReport {
                kind,
                path,
                state: LayerState::SafeMode,
            });
            continue;
        }
        let entries = read_layer(kind, &path, &mut cfg);
        layers.push((kind, path, entries));
    }
    if let Some(path) = paths.workspace_file().filter(|_| safe.is_some()) {
        cfg.layers.push(LayerReport {
            kind: LayerKind::Workspace,
            path,
            state: LayerState::SafeMode,
        });
    } else if let Some(path) = paths.workspace_file() {
        let root = paths.workspace_root.clone().unwrap_or_default();
        let entries = match workspace_gate(&root, &path, paths.trust_dir.as_deref()) {
            Ok(bytes) => parse_workspace_bytes(&path, &bytes, &mut cfg),
            Err(state) => {
                if state != LayerState::Absent {
                    cfg.diagnostics.push(Diagnostic {
                        code: "config.workspace_inert",
                        message: format!(
                            "workspace config {} is {}: not loaded (:trust to load it)",
                            short_path(&path),
                            state.label()
                        ),
                    });
                }
                cfg.layers.push(LayerReport {
                    kind: LayerKind::Workspace,
                    path: path.clone(),
                    state,
                });
                Vec::new()
            }
        };
        layers.push((LayerKind::Workspace, path, entries));
    }
    for def in SCHEMA {
        let contributions: Vec<(Value, Source)> = layers
            .iter()
            .flat_map(|(kind, path, entries)| {
                entries.iter().filter(|e| e.def.key == def.key).map(|e| {
                    (
                        e.value.clone(),
                        Source {
                            layer: *kind,
                            path: path.clone(),
                            line: e.line,
                        },
                    )
                })
            })
            .collect();
        if let Some(eff) = merge(def, &contributions) {
            cfg.values.insert(def.key, eff);
        }
        if !contributions.is_empty() {
            cfg.contributions.insert(def.key, contributions);
        }
    }
    cfg
}

/// Whether the workspace layer at `file` (under `root`) may load: present, not a symbolic link, and
/// trusted with a matching content hash. `Ok` holds the `config.toml` bytes the hash covered, which are
/// the only bytes the layer parses (no second read, so nothing can be swapped in after the check). The
/// error is the inert state to report.
fn workspace_gate(
    root: &Path,
    file: &Path,
    trust_dir: Option<&Path>,
) -> Result<Vec<u8>, LayerState> {
    let dir = root.join(".ruse");
    match std::fs::symlink_metadata(&dir) {
        Err(_) => return Err(LayerState::Absent),
        Ok(m) if m.file_type().is_symlink() => {
            return Err(LayerState::Quarantined(
                ".ruse is a symbolic link; not followed".into(),
            ))
        }
        Ok(_) => {}
    }
    if std::fs::symlink_metadata(file).is_err() {
        return Err(LayerState::Absent);
    }
    let Some(trust_dir) = trust_dir else {
        return Err(LayerState::Untrusted);
    };
    let Some(recorded) = crate::trust::TrustStore::new(trust_dir.to_path_buf()).lookup(root) else {
        return Err(LayerState::Untrusted);
    };
    match crate::trust::ruse_dir_snapshot(root) {
        Ok(snap) if snap.hash == recorded => snap.config.ok_or_else(|| {
            LayerState::Quarantined("not a regular file (a symbolic link is not followed)".into())
        }),
        _ => Err(LayerState::Stale),
    }
}

/// Parse the trusted workspace layer from the bytes [`workspace_gate`] hashed.
fn parse_workspace_bytes(path: &Path, bytes: &[u8], cfg: &mut Config) -> Vec<Entry> {
    let kind = LayerKind::Workspace;
    let fail = |cfg: &mut Config, why: String| {
        cfg.diagnostics.push(quarantined(path, &why));
        cfg.layers.push(LayerReport {
            kind,
            path: path.to_path_buf(),
            state: LayerState::Quarantined(why),
        });
        Vec::new()
    };
    if bytes.len() as u64 > MAX_FILE {
        return fail(cfg, format!("larger than {MAX_FILE} bytes"));
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return fail(cfg, "stream did not contain valid UTF-8".into());
    };
    match parse_layer(kind, path, text, &mut cfg.diagnostics) {
        Ok(entries) => {
            cfg.layers.push(LayerReport {
                kind,
                path: path.to_path_buf(),
                state: LayerState::Loaded,
            });
            entries
        }
        Err(rejected) => fail(cfg, rejected.to_string()),
    }
}

/// Read and parse one layer's file, reporting it and its diagnostics into `cfg`.
fn read_layer(kind: LayerKind, path: &Path, cfg: &mut Config) -> Vec<Entry> {
    let mut report = |state| {
        cfg.layers.push(LayerReport {
            kind,
            path: path.to_path_buf(),
            state,
        });
    };
    let meta = match std::fs::symlink_metadata(path) {
        Err(_) => {
            report(LayerState::Absent);
            return Vec::new();
        }
        Ok(m) => m,
    };
    // (The workspace layer never comes here: it parses the bytes its trust hash covered,
    // [`parse_workspace_bytes`].)
    if meta.len() > MAX_FILE {
        let why = format!("larger than {MAX_FILE} bytes");
        cfg.diagnostics.push(quarantined(path, &why));
        report(LayerState::Quarantined(why));
        return Vec::new();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            let why = e.to_string();
            cfg.diagnostics.push(quarantined(path, &why));
            report(LayerState::Quarantined(why));
            return Vec::new();
        }
    };
    match parse_layer(kind, path, &text, &mut cfg.diagnostics) {
        Ok(entries) => {
            report(LayerState::Loaded);
            entries
        }
        Err(rejected) => {
            let why = rejected.to_string();
            cfg.diagnostics.push(quarantined(path, &why));
            report(LayerState::Quarantined(why));
            Vec::new()
        }
    }
}

/// `path` as a message shows it: relative to the working directory when inside it (`.ruse/config.toml`),
/// else with the home directory as `~`, else absolute.
#[must_use]
pub fn short_path(path: &Path) -> String {
    if let Some(rel) = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
    {
        return rel.display().to_string();
    }
    if let Some(rel) = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf))
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

fn quarantined(path: &Path, why: &str) -> Diagnostic {
    Diagnostic {
        code: "config.layer_quarantined",
        message: format!("{}: {why}; the file is ignored", short_path(path)),
    }
}

/// The 1-based line of byte `at` in `text`.
fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// The config schema version this ruse reads (`version` in `spec/config-schema.yaml`). A layer file may
/// name the version it was written for with a top-level `version = N`; a file without one is taken to be
/// current.
pub const SCHEMA_VERSION: i64 = 1;

/// One change of a schema version step (CFG-MIG-001, `config-model.md` Migration strategy).
// Only the tests build one until the schema's first version step ([`MIGRATIONS`] is empty).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// Auto-conversion: a lossless rename. The old key's value loads as the new key, with a
    /// `config.migrated` note; the file on disk is left as it is.
    Rename {
        from: &'static str,
        to: &'static str,
    },
    /// Manual-warning: a key removed with no equivalent, or changed in meaning. Its value is kept inert
    /// and a `config.migrated` (manual) diagnostic says what to do instead.
    Manual {
        key: &'static str,
        note: &'static str,
    },
}

/// The changes from schema version `from` to `from + 1`.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub from: i64,
    pub changes: &'static [Change],
}

/// A schema version and the steps that lead to it, oldest first.
#[derive(Clone, Copy, Debug)]
pub struct Migrations {
    pub current: i64,
    pub steps: &'static [Migration],
}

/// The migration table. No step: the schema is still at its first version, and no key has been renamed
/// or removed since it was introduced (the history of `spec/config-schema.yaml` only adds keys and edits
/// their rows). A rename or removal bumps [`SCHEMA_VERSION`] and adds its step here and to the schema's
/// `migrations:`.
pub const MIGRATIONS: Migrations = Migrations {
    current: SCHEMA_VERSION,
    steps: &[],
};

impl Migrations {
    /// The steps a layer written for `version` goes through.
    fn from(&self, version: i64) -> impl Iterator<Item = &'static Migration> + '_ {
        self.steps
            .iter()
            .filter(move |m| m.from >= version && m.from < self.current)
    }

    /// What the steps from `version` make of `name`: the key to load it as, or the manual note.
    fn name(&self, name: &str, version: i64) -> Result<String, &'static str> {
        let mut name = name.to_string();
        for step in self.from(version) {
            for change in step.changes {
                match *change {
                    Change::Rename { from, to } if from == name => name = to.to_string(),
                    Change::Manual { key, note } if key == name => return Err(note),
                    _ => {}
                }
            }
        }
        Ok(name)
    }

    /// Whether some key the steps from `version` know by an old name starts with `dotted`.
    fn knows_prefix(&self, dotted: &str, version: i64) -> bool {
        self.from(version)
            .flat_map(|m| m.changes)
            .any(|c| match *c {
                Change::Rename { from, .. } => from.starts_with(dotted),
                Change::Manual { key, .. } => key.starts_with(dotted),
            })
    }

    /// The layer's `version`: its top-level `version = N`, or the current one without it. A version that
    /// is not a positive integer, or newer than this ruse reads, is a diagnostic and read as current.
    fn layer_version(
        &self,
        table: &toml::de::DeTable<'_>,
        text: &str,
        path: &Path,
        diags: &mut Vec<Diagnostic>,
    ) -> i64 {
        let current = self.current;
        let Some((k, v)) = table.iter().find(|(k, _)| k.get_ref() == "version") else {
            return current;
        };
        let at = format!("{}:{}", short_path(path), line_of(text, k.span().start));
        let n = match v.get_ref() {
            toml::de::DeValue::Integer(i) => i64::from_str_radix(i.as_str(), i.radix()).ok(),
            _ => None,
        };
        match n {
            Some(n) if (1..=current).contains(&n) => n,
            Some(n) if n > current => {
                diags.push(Diagnostic {
                    code: "config.migrated",
                    message: format!(
                        "version {n} ({at}) is newer than this ruse reads ({current}); \
                         keys it does not know are ignored"
                    ),
                });
                current
            }
            _ => {
                diags.push(Diagnostic {
                    code: "config.invalid_value",
                    message: format!(
                        "version ({at}): not a version number 1..={current}; read as {current}"
                    ),
                });
                current
            }
        }
    }
}

/// Parse a layer's TOML text into schema entries. A key the schema does not know, one the layer may not
/// set, or a value of the wrong type is a diagnostic and dropped; the rest load. A key under an old
/// name is migrated ([`MIGRATIONS`]). A file that is not TOML is the error (the caller quarantines it).
fn parse_layer(
    kind: LayerKind,
    path: &Path,
    text: &str,
    diags: &mut Vec<Diagnostic>,
) -> Result<Vec<Entry>, Rejected> {
    parse_layer_in(kind, path, text, diags, &MIGRATIONS)
}

/// [`parse_layer`] with the migration table `mig` (a test passes its own).
fn parse_layer_in(
    kind: LayerKind,
    path: &Path,
    text: &str,
    diags: &mut Vec<Diagnostic>,
    mig: &Migrations,
) -> Result<Vec<Entry>, Rejected> {
    let parsed = toml::de::DeTable::parse(text).map_err(|e| Rejected::Syntax {
        line: e.span().map_or(0, |s| line_of(text, s.start)),
        message: e.message().trim_end().to_string(),
    })?;
    let version = mig.layer_version(parsed.get_ref(), text, path, diags);
    let current = mig.current;
    let mut entries = Vec::new();
    let at = |line: usize| format!("{}:{line}", short_path(path));
    let mut stack: Vec<(String, &toml::de::DeTable<'_>)> = vec![(String::new(), parsed.get_ref())];
    while let Some((prefix, t)) = stack.pop() {
        for (k, v) in t {
            if prefix.is_empty() && k.get_ref() == "version" {
                continue;
            }
            let written = if prefix.is_empty() {
                k.get_ref().to_string()
            } else {
                format!("{prefix}.{}", k.get_ref())
            };
            let line = line_of(text, k.span().start);
            let name = match mig.name(&written, version) {
                Ok(name) => name,
                Err(note) => {
                    diags.push(Diagnostic {
                        code: "config.migrated",
                        message: format!(
                            "{written} ({}) is no longer read (manual migration): {note}",
                            at(line)
                        ),
                    });
                    continue;
                }
            };
            if name != written && key_def(&name).is_some() {
                diags.push(Diagnostic {
                    code: "config.migrated",
                    message: format!(
                        "{written} ({}) is now {name}; read as {name} (version {version} \
                         to {current}, the file is unchanged)",
                        at(line)
                    ),
                });
            }
            if let Some(def) = key_def(&name) {
                if !def.scope.allows(kind) || (kind == LayerKind::Workspace && def.locked) {
                    let why = if def.locked && kind == LayerKind::Workspace {
                        "security-locked; a workspace can never set it".to_string()
                    } else {
                        format!(
                            "scope {}; a {} layer cannot set it",
                            def.scope.label(),
                            kind.label()
                        )
                    };
                    diags.push(Diagnostic {
                        code: "config.locked_key_ignored",
                        message: format!("{name} ignored ({}): {why}", at(line)),
                    });
                    continue;
                }
                match convert(def, v.get_ref()) {
                    Ok(value) => entries.push(Entry { def, value, line }),
                    Err(why) => diags.push(Diagnostic {
                        code: "config.invalid_value",
                        message: format!("{name} ({}): {why}; using the layer below", at(line)),
                    }),
                }
                continue;
            }
            let dotted = format!("{written}.");
            match v.get_ref() {
                toml::de::DeValue::Table(sub)
                    if SCHEMA.iter().any(|d| d.key.starts_with(&dotted))
                        || mig.knows_prefix(&dotted, version) =>
                {
                    stack.push((written, sub));
                }
                _ => diags.push(Diagnostic {
                    code: "config.unknown_key",
                    message: format!("unknown key {written} ({}); ignored", at(line)),
                }),
            }
        }
    }
    entries.sort_by_key(|e| e.line);
    Ok(entries)
}

/// Why a layer file or one of its values is rejected.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Rejected {
    /// The file is not TOML.
    Syntax { line: usize, message: String },
    /// A value of the wrong type.
    WrongType {
        expected: &'static str,
        found: &'static str,
    },
    /// An integer past `i64`.
    TooLarge(String),
    /// An integer outside the key's bounds.
    OutOfRange { value: i64, min: i64, max: i64 },
    /// A string that is none of the key's values.
    NotInEnum {
        value: String,
        values: &'static [&'static str],
    },
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejected::Syntax { line, message } => write!(f, "line {line}: {message}"),
            Rejected::WrongType { expected, found } => {
                write!(f, "expected {expected}, found {found}")
            }
            Rejected::TooLarge(n) => write!(f, "{n} is too large"),
            Rejected::OutOfRange { value, min, max } => {
                write!(f, "{value} is outside {min}..={max}")
            }
            Rejected::NotInEnum { value, values } => {
                write!(f, "{value:?} is not one of {}", values.join(", "))
            }
        }
    }
}

/// A TOML value as `def`'s type, or why it is not one.
fn convert(def: &KeyDef, v: &toml::de::DeValue<'_>) -> Result<Value, Rejected> {
    use toml::de::DeValue;
    let want = |expected: &'static str| Rejected::WrongType {
        expected,
        found: v.type_str(),
    };
    match def.ty {
        Ty::Bool => v
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| want("a boolean")),
        Ty::Int { min, max } => {
            let DeValue::Integer(i) = v else {
                return Err(want("an integer"));
            };
            let n = i64::from_str_radix(i.as_str(), i.radix())
                .map_err(|_| Rejected::TooLarge(i.to_string()))?;
            if (min..=max).contains(&n) {
                Ok(Value::Int(n))
            } else {
                Err(Rejected::OutOfRange { value: n, min, max })
            }
        }
        Ty::Enum(values) => {
            let s = v.as_str().ok_or_else(|| want("a string"))?;
            if values.contains(&s) {
                Ok(Value::Str(s.to_string()))
            } else {
                Err(Rejected::NotInEnum {
                    value: s.to_string(),
                    values,
                })
            }
        }
        Ty::Str => v
            .as_str()
            .map(|s| Value::Str(s.to_string()))
            .ok_or_else(|| want("a string")),
        Ty::List => {
            let DeValue::Array(a) = v else {
                return Err(want("an array of strings"));
            };
            a.iter()
                .map(|e| {
                    e.get_ref()
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| want("an array of strings"))
                })
                .collect::<Result<_, _>>()
                .map(Value::List)
        }
        Ty::Map => map_value(v).ok_or_else(|| want("a table")),
    }
}

/// A TOML table as a [`Value::Map`] (strings, booleans, integers, string arrays and tables inside).
fn map_value(v: &toml::de::DeValue<'_>) -> Option<Value> {
    use toml::de::DeValue;
    Some(match v {
        DeValue::Table(t) => Value::Map(
            t.iter()
                .map(|(k, v)| Some((k.get_ref().to_string(), map_value(v.get_ref())?)))
                .collect::<Option<_>>()?,
        ),
        DeValue::String(s) => Value::Str(s.to_string()),
        DeValue::Boolean(b) => Value::Bool(*b),
        DeValue::Integer(i) => Value::Int(i64::from_str_radix(i.as_str(), i.radix()).ok()?),
        DeValue::Array(a) => Value::List(
            a.iter()
                .map(|e| e.get_ref().as_str().map(str::to_string))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

/// Fold `contributions` (lowest layer first) by `def`'s merge strategy.
fn merge(def: &KeyDef, contributions: &[(Value, Source)]) -> Option<Effective> {
    let last = contributions.last()?;
    match def.merge {
        Merge::Replace => {
            // A machine value pins a locked or machine-scoped key above the user and the workspace.
            let pinned = (def.locked || def.scope == Scope::Machine)
                .then(|| {
                    contributions
                        .iter()
                        .find(|(_, s)| s.layer == LayerKind::Machine)
                })
                .flatten();
            let (value, source) = pinned.unwrap_or(last);
            Some(Effective {
                value: value.clone(),
                source: source.clone(),
            })
        }
        Merge::Append | Merge::SetUnion => {
            let mut out: Vec<String> = def.list_default.iter().map(|s| (*s).to_string()).collect();
            for (v, _) in contributions {
                let Value::List(items) = v else { continue };
                for item in items {
                    if let Some(gone) = item.strip_prefix('!') {
                        out.retain(|x| x != gone);
                    } else {
                        out.push(item.clone());
                    }
                }
            }
            if def.merge == Merge::SetUnion {
                out.sort();
                out.dedup();
            }
            Some(Effective {
                value: Value::List(out),
                source: last.1.clone(),
            })
        }
        Merge::Deep => {
            let mut out = BTreeMap::new();
            for (v, _) in contributions {
                if let Value::Map(m) = v {
                    deep_merge(&mut out, m);
                }
            }
            Some(Effective {
                value: Value::Map(out),
                source: last.1.clone(),
            })
        }
    }
}

fn deep_merge(into: &mut BTreeMap<String, Value>, from: &BTreeMap<String, Value>) {
    for (k, v) in from {
        match (into.get_mut(k), v) {
            (Some(Value::Map(a)), Value::Map(b)) => deep_merge(a, b),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

/// `:inspect config` (no key): the layers and every key a layer set, with its source.
#[must_use]
pub fn inspect_all(cfg: &Config) -> Vec<String> {
    let mut rows = vec!["--- Config layers ---".to_string()];
    if let Some(safe) = cfg.safe_mode {
        rows.push(format!(
            "  safe mode ({}): user and workspace layers not read; :config-reload leaves it",
            safe.reason()
        ));
    }
    for l in &cfg.layers {
        let extra = match &l.state {
            LayerState::Quarantined(why) => format!(" ({why})"),
            _ => String::new(),
        };
        rows.push(format!(
            "  {:<9} {:<11} {}{extra}",
            l.kind.label(),
            l.state.label(),
            short_path(&l.path)
        ));
    }
    if !cfg.values.is_empty() {
        rows.push("--- Config values ---".to_string());
        for (k, e) in &cfg.values {
            rows.push(format!(
                "  {k} = {}  ← {} {}:{}",
                e.value,
                e.source.layer.label(),
                short_path(&e.source.path),
                e.source.line
            ));
        }
    }
    if !cfg.diagnostics.is_empty() {
        rows.push("--- Config diagnostics ---".to_string());
        rows.extend(
            cfg.diagnostics
                .iter()
                .map(|d| format!("  {}: {}", d.code, d.message)),
        );
    }
    rows
}

/// `:inspect config {key}`: the effective value and every layer's contribution, or `None` for a key the
/// schema does not know.
#[must_use]
pub fn inspect_key(cfg: &Config, name: &str) -> Option<Vec<String>> {
    let def = key_def(name)?;
    let mut rows = vec![match cfg.get(name) {
        Some(e) => format!("{name} = {}", e.value),
        None => format!("{name} = (schema default)"),
    }];
    rows.push(format!(
        "  scope={} merge={}{}",
        def.scope.label(),
        def.merge.label(),
        if def.locked { " security_locked" } else { "" }
    ));
    for (v, s) in cfg.contributions.get(name).into_iter().flatten() {
        rows.push(format!(
            "  {:<9} {}:{} = {v}",
            s.layer.label(),
            short_path(&s.path),
            s.line
        ));
    }
    Some(rows)
}

/// The one-line startup notice for the diagnostics, if any (shown once; `:inspect config` lists them).
#[must_use]
pub fn startup_notice(cfg: &Config) -> Option<String> {
    if let Some(safe) = cfg.safe_mode {
        return Some(format!(
            "Config: safe-mode ({}): no user or workspace config (:config-reload leaves it)",
            safe.reason()
        ));
    }
    let first = cfg.diagnostics.first()?;
    let more = cfg.diagnostics.len() - 1;
    Some(if more == 0 {
        format!("config: {} (:inspect config)", first.message)
    } else {
        format!("config: {} (+{more} more; :inspect config)", first.message)
    })
}

/// The health of the config for `:checkhealth` (`config-model.md` Observability): `None` when every
/// layer loaded without a diagnostic, else what is wrong (`safe-mode`, or `degraded (N diagnostics)`).
#[must_use]
pub fn health_issue(cfg: &Config) -> Option<String> {
    if let Some(safe) = cfg.safe_mode {
        return Some(format!("safe-mode ({})", safe.reason()));
    }
    let quarantined = cfg
        .layers
        .iter()
        .filter(|l| matches!(l.state, LayerState::Quarantined(_)))
        .count();
    match (cfg.diagnostics.len(), quarantined) {
        (0, 0) => None,
        (n, 0) => Some(format!("degraded ({n} diagnostic(s); :inspect config)")),
        (n, q) => Some(format!(
            "degraded ({q} layer(s) quarantined, {n} diagnostic(s); :inspect config)"
        )),
    }
}

/// Whether a user or workspace layer was read: only then can a start that dies before its first frame
/// be laid at the config's door ([`boot`]).
#[must_use]
pub fn reads_user_layers(cfg: &Config) -> bool {
    cfg.layers
        .iter()
        .any(|l| l.kind != LayerKind::Machine && l.state == LayerState::Loaded)
}

/// The keys whose effective value differs between `old` and `new` (set in one and not the other, or set
/// to another value), in schema order: what a reload applies.
#[must_use]
pub fn changed_keys(old: &Config, new: &Config) -> Vec<&'static str> {
    SCHEMA
        .iter()
        .map(|d| d.key)
        .filter(|k| old.get(k).map(|e| &e.value) != new.get(k).map(|e| &e.value))
        .collect()
}

/// Whether a reload may change `key` in a running session. A `security_locked` key (executables, trust,
/// forwarding) and `input.profile` (the key engine is built once) take effect at the next start: a file
/// edited behind a running session must not loosen its trust (`config-model.md` Open questions, leaning
/// restart-only).
#[must_use]
pub fn reloads_live(key: &str) -> bool {
    key != "input.profile" && key_def(key).is_some_and(|d| !d.locked)
}

/// What the layer files look like on disk, to tell when one changed: for each of the machine, user and
/// workspace files, the `.ruse/` directory and the workspace's trust record, its length, modification
/// time and inode (or nothing when it is absent). Cheap: a few `lstat`s.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Stamp(Vec<Option<(u64, Option<std::time::SystemTime>, u64)>>);

impl Stamp {
    #[must_use]
    pub fn of(paths: &ConfigPaths) -> Stamp {
        let record = paths.workspace_root.as_ref().and_then(|root| {
            Some(crate::trust::TrustStore::new(paths.trust_dir.clone()?).record_path(root))
        });
        let files = [
            paths.machine.clone(),
            paths.user.clone(),
            paths.workspace_file(),
            paths.workspace_root.as_ref().map(|r| r.join(".ruse")),
            record,
        ];
        Stamp(
            files
                .iter()
                .map(|p| {
                    let m = std::fs::symlink_metadata(p.as_ref()?).ok()?;
                    #[cfg(unix)]
                    let ino = std::os::unix::fs::MetadataExt::ino(&m);
                    #[cfg(not(unix))]
                    let ino = 0;
                    Some((m.len(), m.modified().ok(), ino))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests;
