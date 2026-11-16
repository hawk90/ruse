//! Reading the layers: the size bound, the workspace trust gate, quarantine of a file that does not
//! parse, and the fold of every layer's entries into a [`Config`].

use std::path::{Path, PathBuf};

use super::merge::merge;
use super::parse::parse_layer;
use super::{
    short_path, Config, ConfigPaths, Diagnostic, Entry, LayerKind, LayerReport, LayerState,
    SafeMode, Source, Value, SCHEMA,
};

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
            report(&mut cfg, kind, &path, LayerState::SafeMode);
            continue;
        }
        let entries = read_layer(kind, &path, &mut cfg);
        layers.push((kind, path, entries));
    }
    if let Some(path) = paths.workspace_file().filter(|_| safe.is_some()) {
        report(&mut cfg, LayerKind::Workspace, &path, LayerState::SafeMode);
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
                report(&mut cfg, LayerKind::Workspace, &path, state);
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
    if bytes.len() as u64 > MAX_FILE {
        return quarantine(cfg, kind, path, format!("larger than {MAX_FILE} bytes"));
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return quarantine(cfg, kind, path, "stream did not contain valid UTF-8".into());
    };
    match parse_layer(kind, path, text, &mut cfg.diagnostics) {
        Ok(entries) => {
            report(cfg, kind, path, LayerState::Loaded);
            entries
        }
        Err(rejected) => quarantine(cfg, kind, path, rejected.to_string()),
    }
}

/// Read and parse one layer's file, reporting it and its diagnostics into `cfg`.
fn read_layer(kind: LayerKind, path: &Path, cfg: &mut Config) -> Vec<Entry> {
    let meta = match std::fs::symlink_metadata(path) {
        Err(_) => {
            report(cfg, kind, path, LayerState::Absent);
            return Vec::new();
        }
        Ok(m) => m,
    };
    // (The workspace layer never comes here: it parses the bytes its trust hash covered,
    // [`parse_workspace_bytes`].)
    if meta.len() > MAX_FILE {
        return quarantine(cfg, kind, path, format!("larger than {MAX_FILE} bytes"));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return quarantine(cfg, kind, path, e.to_string()),
    };
    match parse_layer(kind, path, &text, &mut cfg.diagnostics) {
        Ok(entries) => {
            report(cfg, kind, path, LayerState::Loaded);
            entries
        }
        Err(rejected) => quarantine(cfg, kind, path, rejected.to_string()),
    }
}

/// Record what became of the layer `kind` at `path`.
fn report(cfg: &mut Config, kind: LayerKind, path: &Path, state: LayerState) {
    cfg.layers.push(LayerReport {
        kind,
        path: path.to_path_buf(),
        state,
    });
}

/// Quarantine the layer `kind` at `path` for `why`: its diagnostic, its report, and no entries.
fn quarantine(cfg: &mut Config, kind: LayerKind, path: &Path, why: String) -> Vec<Entry> {
    cfg.diagnostics.push(quarantined(path, &why));
    report(cfg, kind, path, LayerState::Quarantined(why));
    Vec::new()
}

fn quarantined(path: &Path, why: &str) -> Diagnostic {
    Diagnostic {
        code: "config.layer_quarantined",
        message: format!("{}: {why}; the file is ignored", short_path(path)),
    }
}

/// The 1-based line of byte `at` in `text`.
pub(super) fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}
