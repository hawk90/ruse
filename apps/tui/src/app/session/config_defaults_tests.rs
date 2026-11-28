//! #1322: every `default:` of `spec/config-schema.yaml` is the default ruse actually runs with.
//!
//! The check writes each key's spec default into a machine layer, loads it, and installs it the way a
//! start does. A key whose spec default is the runtime default leaves everything as an unconfigured
//! session has it, so the representations (`editor.indent_style = "space"` for `'expandtab'`, an Emacs
//! `nil` for a `None`, a 256 MiB budget as a byte count) are compared by the code that reads them, not
//! by a second copy of that mapping here.

use std::collections::BTreeSet;

use super::{Session, SessionInit};
use crate::app::set_exec::{config_key, default_of, OptSource, OptionHost};
use crate::app::write_opts::EmacsFiles;
use crate::config::tests::spec_settings;
use crate::config::{key_def, Config, ConfigPaths, KeyDef, Merge, Ty};
use crate::input::set_cmd::{OptVal, OPTIONS};

/// Spec keys whose default is known to differ from the runtime default, each with the follow-up that
/// settles it. The test fails when a listed key starts matching, so the list can only shrink.
const KNOWN_MISMATCHES: &[(&str, &str)] = &[
    // The spec says `nil` (Emacs's global value), but ruse leaves the key unset and a buffer follows
    // `emacs.mode_require_final_newline` (t); a config that writes the spec default `nil` turns the
    // final newline off. Either the spec default or the reader changes: #1701.
    ("emacs.require_final_newline", "#1701: unset is not nil"),
];

/// Spec keys no runtime code reads yet, so they have no runtime default to compare: the loader still
/// checks the spec default below (it must load without a diagnostic). A key leaves this list when code
/// starts reading it, and must then join one of the checks.
const NO_READER: &[&str] = &[
    // Read and type-checked, not installed: ruse has no user mappings for these namespaces yet.
    "keymap.normal",
    "keymap.operator_pending",
    "keymap.insert",
    "keymap.cmdline",
    "keymap.visual",
    "keymap.select",
    "keymap.terminal",
    "editor.line_endings",
    "files.watcher",
    "files.exclude",
    "ui.theme",
    "update.channel",
    "remote.trust_prompt",
    "plugins.allow_unsigned",
    "telemetry.enabled",
];

/// The keys read by a dedicated reader rather than an option: each has its own check below.
const OWN_READER: &[&str] = &[
    "input.profile",
    "workspace.trusted_roots",
    "keymap.lang",
    "render.conceal",
    "editor.undo_max_bytes",
];

fn session(dir: &crate::TestDir) -> Session {
    Session::new(SessionInit {
        path: None,
        fmt: crate::persist::encoding::FileFormat::default(),
        initial: Vec::new(),
        status: String::new(),
        profile: "vim",
        term: super::TermInfo::default(),
        cwd: dir.to_path_buf(),
        trusted: false,
        rescue: false,
    })
}

/// `opt`'s value in `s`: the global copy, and the focused window's.
fn option(s: &mut Session, opt: crate::input::set_cmd::Opt) -> (OptVal, OptVal) {
    let Session {
        ed:
            super::Editing {
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
        readonly,
        ..
    } = s;
    let host = OptionHost {
        ws,
        recorded,
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
        screen_rows: 0,
    };
    (host.get(opt, true), host.get(opt, false))
}

/// A spec default as a TOML value: a bare YAML scalar of a string key gets quotes; a double-quoted
/// YAML string uses the escapes a TOML basic string does (`\\`, `\"`).
fn toml_value(def: &KeyDef, raw: &str) -> String {
    match def.ty {
        Ty::Str | Ty::Enum(_) if !raw.starts_with('"') => format!("\"{raw}\""),
        _ => raw.to_string(),
    }
}

/// The items of a flow list `["a", "b"]`.
fn list_items(raw: &str) -> Vec<String> {
    let inner = raw.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The options whose spec default is a template the runtime expands at startup (nvim's spelling, with
/// ruse's state directory): compared after [`expand_template`], not by installing the template.
const ENV_TEMPLATES: &[&str] = &["files.backupdir", "files.backupskip"];

/// A comma list as ruse expands it at startup: `$XDG_STATE_HOME` is the state home (else
/// `$HOME/.local/state`), another `$VAR` its value; an item whose variable is unset goes. A `dir/*`
/// item loses the trailing `/` of its directory, a literal `/tmp` is the real `/private/tmp` on macOS,
/// and a repeated item goes.
fn expand_template(raw: &str) -> String {
    let var = |n: &str| {
        let v = std::env::var_os(n).filter(|v| !v.is_empty());
        match n {
            "XDG_STATE_HOME" => v
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|h| std::path::PathBuf::from(h).join(".local/state"))
                })
                .map(|p| p.display().to_string()),
            _ => v.map(|v| v.to_string_lossy().into_owned()),
        }
    };
    let mut out: Vec<String> = Vec::new();
    'items: for item in raw.trim_matches('"').split(',') {
        let literal = !item.contains('$');
        let mut s = String::new();
        let mut rest = item;
        while let Some(i) = rest.find('$') {
            s.push_str(&rest[..i]);
            let tail = &rest[i + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let Some(v) = var(&tail[..end]) else {
                continue 'items;
            };
            s.push_str(&v);
            rest = &tail[end..];
        }
        s.push_str(rest);
        if let Some(d) = s.strip_suffix("/*") {
            let d = d.trim_end_matches('/');
            let d = if cfg!(target_os = "macos") && literal && d == "/tmp" {
                "/private/tmp"
            } else {
                d
            };
            s = format!("{d}/*");
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out.join(",")
}

/// `cfg` with `key` alone.
fn only(cfg: &Config, key: &str) -> Config {
    let mut one = cfg.clone();
    one.values.retain(|k, _| *k == key);
    one
}

#[test]
fn every_spec_default_is_the_runtime_default() {
    let spec: Vec<(String, String)> = spec_settings()
        .into_iter()
        .map(|(k, fields)| {
            let raw = fields
                .into_iter()
                .find(|(n, _)| n == "default")
                .unwrap_or_else(|| panic!("{k} has no default"))
                .1;
            (k, raw)
        })
        .collect();
    let dir = crate::TestDir::new("config-defaults");
    // A list key folds the layers onto its schema default (`append`, `set-union`), so writing the
    // default into a layer would double it: its default is the schema's `list_default`, compared as is.
    let mut mismatches: Vec<(String, String)> = Vec::new();
    let mut layer = String::new();
    for (key, raw) in &spec {
        let def = key_def(key).unwrap_or_else(|| panic!("{key} missing from SCHEMA"));
        if matches!(def.merge, Merge::Append | Merge::SetUnion) {
            if def.list_default != list_items(raw).as_slice() {
                mismatches.push((
                    key.clone(),
                    format!("spec {raw}, schema list_default {:?}", def.list_default),
                ));
            }
            continue;
        }
        layer.push_str(&format!("{key} = {}\n", toml_value(def, raw)));
    }
    let machine = dir.join("machine.toml");
    std::fs::write(&machine, &layer).unwrap();
    let cfg = crate::config::load_mode(
        &ConfigPaths {
            machine: Some(machine),
            ..ConfigPaths::default()
        },
        None,
    );
    assert!(
        cfg.diagnostics.is_empty(),
        "a spec default the loader refuses: {:?}",
        cfg.diagnostics
    );

    // The options: a session that installed the spec defaults holds what an unconfigured one does, and
    // that is the default `:set opt&` resets to.
    let mut fresh = session(&dir);
    let mut configured = session(&dir);
    configured.config = cfg.clone();
    configured.apply_config(true);
    let mut covered: BTreeSet<&str> = BTreeSet::new();
    for def in OPTIONS {
        let Some(key) = config_key(def.opt) else {
            continue;
        };
        assert!(
            covered.insert(key),
            "{key} is the config key of two options"
        );
        if !matches!(
            configured.opt_sources.get(&def.opt),
            Some(OptSource::Config { .. })
        ) {
            mismatches.push((key.into(), "the option refuses the spec default".into()));
            continue;
        }
        let want = option(&mut fresh, def.opt);
        let got = match ENV_TEMPLATES.contains(&key) {
            true => {
                let raw = &spec.iter().find(|(k, _)| k == key).expect("spec key").1;
                let v = OptVal::Text(expand_template(raw).into());
                (v.clone(), v)
            }
            false => option(&mut configured, def.opt),
        };
        if got != want {
            mismatches.push((
                key.into(),
                format!("spec default gives {got:?}, the runtime default is {want:?}"),
            ));
        } else if default_of(def) != want.0 {
            mismatches.push((
                key.into(),
                format!(
                    "`:set {}&` resets to {:?}, a session starts with {:?}",
                    def.name,
                    default_of(def),
                    want.0
                ),
            ));
        }
    }

    // The keys with a reader of their own.
    let none = Config::default();
    let mut own = |key: &str, same: bool, detail: String| {
        if !same {
            mismatches.push((key.into(), detail));
        }
    };
    own(
        "input.profile",
        super::event_loop::startup_profile(None, &cfg)
            == super::event_loop::startup_profile(None, &none),
        format!("profile {}", super::event_loop::startup_profile(None, &cfg)),
    );
    own(
        "workspace.trusted_roots",
        super::event_loop::configured_roots(&cfg) == super::event_loop::configured_roots(&none),
        format!("{:?}", super::event_loop::configured_roots(&cfg)),
    );
    let mut installed = Vec::new();
    let notes = crate::app::set_exec::install_lang_keymap(&cfg, &mut fresh.engine, &mut installed);
    own(
        "keymap.lang",
        installed.is_empty() && notes.is_empty(),
        format!("installs {installed:?}, notes {notes:?}"),
    );
    own(
        "render.conceal",
        configured.ui_opts.conceal == fresh.ui_opts.conceal,
        format!("conceal {}", configured.ui_opts.conceal),
    );
    own(
        "editor.undo_max_bytes",
        configured.ui_opts.undo_max_bytes == fresh.ui_opts.undo_max_bytes,
        format!(
            "spec {}, runtime {}",
            configured.ui_opts.undo_max_bytes, fresh.ui_opts.undo_max_bytes
        ),
    );

    // The Emacs profile's variables, one key at a time so a mismatch names its key.
    for (key, _) in spec.iter().filter(|(k, _)| k.starts_with("emacs.")) {
        let got = EmacsFiles::from_config(&only(&cfg, key));
        if got != EmacsFiles::default() {
            mismatches.push((key.clone(), format!("gives {got:?}")));
        }
    }

    // Every spec key is checked by exactly one of the above (a list key by its `list_default`, too).
    for (key, _) in &spec {
        let homes = [
            covered.contains(key.as_str()),
            OWN_READER.contains(&key.as_str()),
            key.starts_with("emacs."),
            NO_READER.contains(&key.as_str()),
        ];
        assert_eq!(
            homes.iter().filter(|h| **h).count(),
            1,
            "{key}: not covered by exactly one default check"
        );
    }
    for key in OWN_READER.iter().chain(NO_READER) {
        assert!(
            spec.iter().any(|(k, _)| k == key),
            "{key} is not a spec key"
        );
    }

    let found: BTreeSet<&str> = mismatches.iter().map(|(k, _)| k.as_str()).collect();
    let known: BTreeSet<&str> = KNOWN_MISMATCHES.iter().map(|(k, _)| *k).collect();
    let new: Vec<_> = mismatches
        .iter()
        .filter(|(k, _)| !known.contains(k.as_str()))
        .collect();
    assert!(
        new.is_empty(),
        "spec default differs from the runtime default: {new:#?}"
    );
    let fixed: Vec<_> = known.difference(&found).collect();
    assert!(
        fixed.is_empty(),
        "now matching, drop from KNOWN_MISMATCHES: {fixed:?}"
    );
}
