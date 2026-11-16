//! Schema-version migrations (CFG-MIG-001): renamed keys load under their new name, removed keys stay
//! inert with a warning.

use std::path::Path;

use super::load::line_of;
use super::{short_path, Diagnostic};

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
    pub(super) fn name(&self, name: &str, version: i64) -> Result<String, &'static str> {
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
    pub(super) fn knows_prefix(&self, dotted: &str, version: i64) -> bool {
        self.from(version)
            .flat_map(|m| m.changes)
            .any(|c| match *c {
                Change::Rename { from, .. } => from.starts_with(dotted),
                Change::Manual { key, .. } => key.starts_with(dotted),
            })
    }

    /// The layer's `version`: its top-level `version = N`, or the current one without it. A version that
    /// is not a positive integer, or newer than this ruse reads, is a diagnostic and read as current.
    pub(super) fn layer_version(
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
