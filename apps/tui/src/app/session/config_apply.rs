//! Installing the config layers into the options, workspace trust of the config, and live reload (#614,
//! #1152). Split from `session.rs` (#1423).

use super::*;

impl Session {
    /// Install the loaded config ([`Session::config`]) into the options, with provenance (#614). At
    /// `startup` nothing goes into the trace and the trace's starting options are read again afterwards
    /// (a trace starts from the configured values, RFC-0020 R4); a reload mid-session records its sets.
    pub(crate) fn apply_config(&mut self, startup: bool) {
        self.apply_config_keys(startup, None);
    }

    /// [`Session::apply_config`] for the keys in `only` alone (every key with `None`): a reload installs
    /// only what changed, so an unchanged key leaves a window's local value alone.
    pub(super) fn apply_config_keys(&mut self, startup: bool, only: Option<&[&'static str]>) {
        let mut scratch = Vec::new();
        let Session {
            ed:
                Editing {
                    ws,
                    recorded,
                    files,
                    ..
                },
            engine,
            hlsearch_on,
            search_hl,
            incsearch_on,
            scrolloff,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            opt_sources,
            ui_opts,
            config,
            readonly,
            ..
        } = self;
        let mut host = crate::app::set_exec::OptionHost {
            ws,
            recorded: if startup { &mut scratch } else { recorded },
            engine,
            hlsearch_on,
            search_hl,
            incsearch_on,
            scrolloff,
            folds,
            fold_method,
            foldlevel,
            foldenable,
            fold_rev,
            sources: opt_sources,
            ui: ui_opts,
            files,
            readonly,
            win_height: 0,
        };
        crate::app::set_exec::apply_config_keys(config, &mut host, only);
        let notes = if only.is_none_or(|k| k.contains(&"keymap.lang")) {
            crate::app::set_exec::install_lang_keymap(config, engine, &mut self.config_lmaps)
        } else {
            Vec::new()
        };
        if !notes.is_empty() {
            let shown = format!("{} · {}", self.ed.status, notes.join(" · "));
            self.ed.status.amend(shown);
        }
        if startup {
            self.start_options = self.ed.ws.focused().view.editing_options();
            self.start_registers = self.ed.ws.registers().filled();
        }
    }

    /// `:trust` (`grant`) / `:untrust`: write or delete the user-side trust record of the workspace root
    /// (its `.ruse/` content hash, D-062), then reload the config so the workspace layer comes in or goes.
    /// `:untrust` also takes back the session's language-server grant (servers already running keep
    /// running until they exit).
    pub(super) fn change_config_trust(&mut self, grant: bool) {
        if !grant {
            self.lsp.revoke_trust();
        }
        let paths = self.config_paths.clone();
        let target = paths.as_ref().and_then(|p| {
            Some((
                p.workspace_root.clone()?,
                crate::trust::TrustStore::new(p.trust_dir.clone()?),
            ))
        });
        let Some((root, store)) = target else {
            if !grant {
                self.ed.status.info("workspace untrusted for this session");
            }
            return;
        };
        let note = if grant {
            match crate::trust::ruse_dir_hash(&root).and_then(|h| store.record(&root, &h)) {
                Ok(_) => None,
                Err(e) => Some(format!("trust record not written: {e}")),
            }
        } else {
            Some(match store.remove(&root) {
                Ok(true) => "workspace untrusted: its config is no longer loaded".to_string(),
                Ok(false) => "workspace untrusted for this session (it had no trust record)".into(),
                Err(e) => format!("trust record not removed: {e}"),
            })
        };
        if let Some(paths) = paths {
            self.config = crate::config::load_mode(&paths, self.config.safe_mode);
            self.config_stamp = crate::config::Stamp::of(&paths);
            self.apply_config(false);
        }
        let workspace_loaded = self.config.layers.iter().any(|l| {
            l.kind == crate::config::LayerKind::Workspace
                && l.state == crate::config::LayerState::Loaded
        });
        match note {
            Some(n) if grant => {
                let shown = format!("{}; {n}", self.ed.status);
                self.ed.status.amend(shown);
            }
            Some(n) => self.ed.status.info(n),
            None if workspace_loaded => {
                let shown = format!("{}; workspace config loaded", self.ed.status);
                self.ed.status.amend(shown);
            }
            None => {}
        }
    }

    /// Reload the config when a layer file changed on disk since it was loaded (#1152). Called where the
    /// session looks at its files again: when idle, on focus gain, after an Ex command.
    pub(crate) fn watch_config(&mut self) {
        let Some(paths) = &self.config_paths else {
            return;
        };
        if crate::config::Stamp::of(paths) != self.config_stamp {
            self.reload_config(false);
        }
    }

    /// Load the config layers again and install what changed (`config-model.md` Performance: the merge
    /// is recomputed and diffed). `explicit` is `:config-reload`, which also leaves safe mode (and clears
    /// the crash-loop record) and always reports; an automatic reload keeps safe mode and is quiet when no
    /// value changed. A layer that loaded before and now does not parse keeps the whole previous config,
    /// and the error is reported. A key that only applies at startup (`input.profile`, security-locked
    /// keys) keeps its running value and is named as taking effect at the next start.
    pub(crate) fn reload_config(&mut self, explicit: bool) {
        use crate::config::LayerState;
        let Some(paths) = self.config_paths.clone() else {
            if explicit {
                self.ed
                    .status
                    .error("config-reload: no config files in this session");
            }
            return;
        };
        let safe = if explicit {
            None
        } else {
            self.config.safe_mode
        };
        let mut new = crate::config::load_mode(&paths, safe);
        self.config_stamp = crate::config::Stamp::of(&paths);
        for d in &new.diagnostics {
            crate::log::config_diagnostic(d.code);
        }
        let broken = new.layers.iter().find(|l| {
            matches!(l.state, LayerState::Quarantined(_))
                && self
                    .config
                    .layers
                    .iter()
                    .any(|o| o.kind == l.kind && o.state == LayerState::Loaded)
        });
        if let Some(l) = broken {
            let why = match &l.state {
                LayerState::Quarantined(why) => why.clone(),
                _ => String::new(),
            };
            let text = format!(
                "config not reloaded, the previous one stays: {}: {why}",
                crate::config::short_path(&l.path)
            );
            if explicit {
                self.ed.status.error(text);
            } else {
                self.ed.status.warn(text);
            }
            return;
        }
        let changed = crate::config::changed_keys(&self.config, &new);
        let (live, later): (Vec<&'static str>, Vec<&'static str>) =
            changed.iter().partition(|k| crate::config::reloads_live(k));
        // A key that applies only at startup keeps its running value in the snapshot.
        for k in &later {
            match self.config.values.get(k) {
                Some(e) => new.values.insert(k, e.clone()),
                None => new.values.remove(k),
            };
        }
        let left_safe = self.config.safe_mode.is_some() && new.safe_mode.is_none();
        self.config = new;
        self.apply_config_keys(false, Some(&live));
        if left_safe {
            if let Some(b) = &self.boot_record {
                b.clear();
            }
        }
        if !explicit && changed.is_empty() {
            return;
        }
        let mut text = match live.len() {
            0 if left_safe => "config reloaded; safe mode off".to_string(),
            0 => "config reloaded: no change".to_string(),
            n => format!(
                "config reloaded{}: {n} key(s) changed ({})",
                if left_safe { "; safe mode off" } else { "" },
                live.join(", ")
            ),
        };
        if !later.is_empty() {
            text.push_str(&format!("; at the next start: {}", later.join(", ")));
        }
        if !self.config.diagnostics.is_empty() {
            text.push_str(&format!(
                " (+{} diagnostic(s); :inspect config)",
                self.config.diagnostics.len()
            ));
        }
        self.ed.status.info(text);
    }
}
