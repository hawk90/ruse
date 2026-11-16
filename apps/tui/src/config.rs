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
mod inspect;
mod load;
mod merge;
mod migrate;
mod parse;
mod paths;
mod reload;
mod schema;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

pub use inspect::{health_issue, inspect_all, inspect_key, reads_user_layers, startup_notice};
pub use load::load_mode;
pub use migrate::{Migrations, MIGRATIONS};
pub use paths::{short_path, ConfigPaths};
pub use reload::{changed_keys, reloads_live, Stamp};
pub use schema::{key_def, KeyDef, LayerKind, Merge, Scope, Ty, SCHEMA};

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

/// One parsed entry of a layer.
#[derive(Clone, Debug)]
struct Entry {
    def: &'static KeyDef,
    value: Value,
    line: usize,
}

#[cfg(test)]
mod tests;
