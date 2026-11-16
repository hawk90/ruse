//! `:inspect config`, the startup notice and the health issue.

use super::{key_def, short_path, Config, LayerKind, LayerState};

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
/// be laid at the config's door ([`boot`](super::boot)).
#[must_use]
pub fn reads_user_layers(cfg: &Config) -> bool {
    cfg.layers
        .iter()
        .any(|l| l.kind != LayerKind::Machine && l.state == LayerState::Loaded)
}
