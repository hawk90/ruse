//! A layer's TOML text to typed schema entries, with the diagnostics for what is dropped.

use std::fmt;
use std::path::Path;

use super::load::line_of;
use super::{
    key_def, short_path, Diagnostic, Entry, KeyDef, LayerKind, Migrations, Ty, Value, MIGRATIONS,
    SCHEMA,
};

/// Parse a layer's TOML text into schema entries. A key the schema does not know, one the layer may not
/// set, or a value of the wrong type is a diagnostic and dropped; the rest load. A key under an old
/// name is migrated ([`MIGRATIONS`]). A file that is not TOML is the error (the caller quarantines it).
pub(super) fn parse_layer(
    kind: LayerKind,
    path: &Path,
    text: &str,
    diags: &mut Vec<Diagnostic>,
) -> Result<Vec<Entry>, Rejected> {
    parse_layer_in(kind, path, text, diags, &MIGRATIONS)
}

/// [`parse_layer`] with the migration table `mig` (a test passes its own).
pub(super) fn parse_layer_in(
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
