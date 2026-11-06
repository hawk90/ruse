//! The config loader: precedence, merge, lock, failure handling and the content-bound workspace gate
//! (`config-model.md` Test strategy), plus the drift check that keeps [`SCHEMA`] equal to
//! `spec/config-schema.yaml`.

use super::*;
use crate::trust::{ruse_dir_hash, TrustStore};

struct Dirs {
    base: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Dirs {
        let base = std::env::temp_dir().join(format!("ruse-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("ws")).unwrap();
        Dirs { base }
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths {
            machine: Some(self.base.join("machine.toml")),
            user: Some(self.base.join("user.toml")),
            workspace_root: Some(self.base.join("ws")),
            trust_dir: Some(self.base.join("trust")),
        }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn trust(&self) {
        let root = self.base.join("ws");
        let hash = ruse_dir_hash(&root).unwrap();
        TrustStore::new(self.base.join("trust"))
            .record(&root, &hash)
            .unwrap();
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn value(cfg: &Config, key: &str) -> Option<Value> {
    cfg.get(key).map(|e| e.value.clone())
}

fn codes(cfg: &Config) -> Vec<&'static str> {
    cfg.diagnostics.iter().map(|d| d.code).collect()
}

fn state(cfg: &Config, kind: LayerKind) -> LayerState {
    cfg.layers
        .iter()
        .find(|l| l.kind == kind)
        .map(|l| l.state.clone())
        .unwrap()
}

#[test]
fn replace_takes_the_highest_allowed_layer() {
    let d = Dirs::new("replace");
    d.write("machine.toml", "[editor]\ntab_width = 8\nscrolloff = 1\n");
    d.write("user.toml", "editor.tab_width = 4\n");
    d.write("ws/.ruse/config.toml", "[editor]\ntab_width = 2\n");
    d.trust();
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "editor.tab_width"), Some(Value::Int(2)));
    let src = &cfg.get("editor.tab_width").unwrap().source;
    assert_eq!((src.layer, src.line), (LayerKind::Workspace, 2));
    assert_eq!(value(&cfg, "editor.scrolloff"), Some(Value::Int(1)));
    assert!(cfg.diagnostics.is_empty(), "{:?}", cfg.diagnostics);
}

#[test]
fn a_layer_broader_than_the_scope_is_inert_for_the_key() {
    let d = Dirs::new("scope");
    // `editor.scrolloff` and the keymaps are scope user; `plugins.allow_unsigned` is scope machine.
    d.write("user.toml", "plugins.allow_unsigned = true\n");
    d.write(
        "ws/.ruse/config.toml",
        "editor.scrolloff = 9\n[keymap.normal]\nx = \"dd\"\n",
    );
    d.trust();
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "plugins.allow_unsigned"), None);
    assert_eq!(value(&cfg, "editor.scrolloff"), None);
    assert_eq!(
        value(&cfg, "keymap.normal"),
        None,
        "a workspace never binds keys (D-062)"
    );
    assert_eq!(
        codes(&cfg),
        ["config.locked_key_ignored"; 3],
        "{:?}",
        cfg.diagnostics
    );
}

#[test]
fn a_workspace_never_sets_a_security_locked_key() {
    let d = Dirs::new("locked");
    d.write(
        "ws/.ruse/config.toml",
        "workspace.trusted_roots = [\"/\"]\nremote.trust_prompt = false\n",
    );
    d.trust();
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "workspace.trusted_roots"), None);
    assert_eq!(value(&cfg, "remote.trust_prompt"), None);
    assert!(cfg
        .diagnostics
        .iter()
        .all(|x| x.code == "config.locked_key_ignored" && x.message.contains("security-locked")));
}

#[test]
fn machine_pins_a_locked_key() {
    let d = Dirs::new("pin");
    d.write("machine.toml", "remote.trust_prompt = true\n");
    d.write("user.toml", "remote.trust_prompt = false\n");
    let cfg = load(&d.paths());
    let e = cfg.get("remote.trust_prompt").unwrap();
    assert_eq!(
        (e.value.clone(), e.source.layer),
        (Value::Bool(true), LayerKind::Machine)
    );
}

#[test]
fn lists_append_union_and_negate() {
    let d = Dirs::new("lists");
    d.write("user.toml", "files.exclude = [\"dist\", \"!node_modules\"]\nworkspace.trusted_roots = [\"/b\", \"/a\", \"/b\"]\n");
    d.write(
        "ws/.ruse/config.toml",
        "files.exclude = [\"build\", \"dist\"]\n",
    );
    d.trust();
    let cfg = load(&d.paths());
    let list = |v: &[&str]| Value::List(v.iter().map(|s| (*s).to_string()).collect());
    // `append` keeps order and duplicates, from the schema default; `!x` removes an earlier element.
    assert_eq!(
        value(&cfg, "files.exclude"),
        Some(list(&[".git", "target", "dist", "build", "dist"]))
    );
    // `set-union` de-duplicates and sorts.
    assert_eq!(
        value(&cfg, "workspace.trusted_roots"),
        Some(list(&["/a", "/b"]))
    );
}

#[test]
fn maps_deep_merge() {
    let d = Dirs::new("maps");
    d.write("machine.toml", "[keymap.normal]\nQ = \"gq\"\nY = \"y$\"\n");
    d.write("user.toml", "[keymap.normal]\nY = \"yy\"\n");
    let cfg = load(&d.paths());
    let Some(Value::Map(m)) = value(&cfg, "keymap.normal") else {
        panic!("{:?}", cfg.values)
    };
    assert_eq!(m.get("Q"), Some(&Value::Str("gq".into())));
    assert_eq!(m.get("Y"), Some(&Value::Str("yy".into())));
}

#[test]
fn one_bad_key_falls_back_and_never_aborts() {
    let d = Dirs::new("bad");
    d.write("machine.toml", "editor.tab_width = 6\n");
    d.write(
        "user.toml",
        "editor.tab_width = 99\neditor.tabwidth = 2\neditor.indent_style = \"tabs\"\neditor.textwidth = 72\neditor.ignorecase = \"yes\"\n",
    );
    let cfg = load(&d.paths());
    // Out of range: the machine layer's value stands.
    assert_eq!(value(&cfg, "editor.tab_width"), Some(Value::Int(6)));
    assert_eq!(value(&cfg, "editor.indent_style"), None);
    assert_eq!(value(&cfg, "editor.ignorecase"), None);
    // The good key in the same file still loads.
    assert_eq!(value(&cfg, "editor.textwidth"), Some(Value::Int(72)));
    let mut c = codes(&cfg);
    c.sort_unstable();
    assert_eq!(
        c,
        [
            "config.invalid_value",
            "config.invalid_value",
            "config.invalid_value",
            "config.unknown_key"
        ]
    );
    let unknown = cfg
        .diagnostics
        .iter()
        .find(|x| x.code == "config.unknown_key")
        .unwrap();
    assert!(unknown.message.contains("editor.tabwidth") && unknown.message.contains(":2"));
    assert!(startup_notice(&cfg).unwrap().contains("+3 more"));
}

#[test]
fn a_malformed_file_is_quarantined_alone() {
    let d = Dirs::new("malformed");
    d.write("machine.toml", "editor.tab_width = 6\n");
    d.write("user.toml", "editor.tab_width = 2\n[editor\n");
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "editor.tab_width"), Some(Value::Int(6)));
    assert!(matches!(
        state(&cfg, LayerKind::User),
        LayerState::Quarantined(_)
    ));
    assert_eq!(codes(&cfg), ["config.layer_quarantined"]);
    assert!(
        cfg.diagnostics[0].message.contains("line 2"),
        "{:?}",
        cfg.diagnostics
    );
}

#[test]
fn an_untrusted_workspace_is_inert() {
    let d = Dirs::new("untrusted");
    d.write("ws/.ruse/config.toml", "editor.tab_width = 2\n");
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "editor.tab_width"), None);
    assert_eq!(state(&cfg, LayerKind::Workspace), LayerState::Untrusted);
    assert_eq!(codes(&cfg), ["config.workspace_inert"]);
}

#[test]
fn a_changed_workspace_is_stale_until_trusted_again() {
    let d = Dirs::new("stale");
    d.write("ws/.ruse/config.toml", "editor.tab_width = 2\n");
    d.trust();
    assert_eq!(
        value(&load(&d.paths()), "editor.tab_width"),
        Some(Value::Int(2))
    );
    // A pull changes the config: stale, inert.
    d.write("ws/.ruse/config.toml", "editor.tab_width = 3\n");
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "editor.tab_width"), None);
    assert_eq!(state(&cfg, LayerKind::Workspace), LayerState::Stale);
    // Any file under `.ruse/` counts, not only config.toml.
    d.trust();
    assert_eq!(
        value(&load(&d.paths()), "editor.tab_width"),
        Some(Value::Int(3))
    );
    d.write("ws/.ruse/other/notes.txt", "x");
    assert_eq!(
        state(&load(&d.paths()), LayerKind::Workspace),
        LayerState::Stale
    );
    // `:untrust` removes the record.
    let root = d.base.join("ws");
    assert!(TrustStore::new(d.base.join("trust")).remove(&root).unwrap());
    assert_eq!(
        state(&load(&d.paths()), LayerKind::Workspace),
        LayerState::Untrusted
    );
}

#[cfg(unix)]
#[test]
fn a_linked_workspace_config_is_not_followed() {
    let d = Dirs::new("link");
    d.write("elsewhere.toml", "editor.tab_width = 2\n");
    std::fs::create_dir_all(d.base.join("ws/.ruse")).unwrap();
    std::os::unix::fs::symlink(
        d.base.join("elsewhere.toml"),
        d.base.join("ws/.ruse/config.toml"),
    )
    .unwrap();
    d.trust();
    let cfg = load(&d.paths());
    assert_eq!(value(&cfg, "editor.tab_width"), None);
    assert!(matches!(
        state(&cfg, LayerKind::Workspace),
        LayerState::Quarantined(_)
    ));
}

#[test]
fn the_snapshot_holds_the_config_bytes_it_hashed() {
    let d = Dirs::new("snap");
    let root = d.base.join("ws");
    d.write("ws/.ruse/config.toml", "[editor]\ntab_width = 3\n");
    d.write("ws/.ruse/other.txt", "x");
    let snap = crate::trust::ruse_dir_snapshot(&root).unwrap();
    assert_eq!(snap.hash, ruse_dir_hash(&root).unwrap());
    assert_eq!(
        snap.config.as_deref(),
        Some(&b"[editor]\ntab_width = 3\n"[..])
    );
    // No config.toml: the hash still covers the directory, and there is nothing to parse.
    std::fs::remove_file(root.join(".ruse/config.toml")).unwrap();
    assert!(crate::trust::ruse_dir_snapshot(&root)
        .unwrap()
        .config
        .is_none());
}

#[test]
fn the_content_hash_is_stable_and_covers_paths_and_bytes() {
    let d = Dirs::new("hash");
    let root = d.base.join("ws");
    let empty = ruse_dir_hash(&root).unwrap();
    assert!(empty.starts_with("sha256:") && empty.len() == 7 + 64);
    d.write("ws/.ruse/a", "1");
    let one = ruse_dir_hash(&root).unwrap();
    assert_ne!(one, empty);
    assert_eq!(ruse_dir_hash(&root).unwrap(), one, "deterministic");
    // Same bytes under another name is another hash.
    std::fs::rename(root.join(".ruse/a"), root.join(".ruse/b")).unwrap();
    assert_ne!(ruse_dir_hash(&root).unwrap(), one);
}

/// Every key of `spec/config-schema.yaml` is in [`SCHEMA`] with the same type, scope, merge, lock and
/// bounds, and the other way round.
#[test]
fn schema_matches_spec() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/config-schema.yaml");
    let text = std::fs::read_to_string(path).unwrap();
    let settings = text.split("\nsettings:\n").nth(1).unwrap();
    let mut seen = Vec::new();
    for line in settings.lines() {
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        if rest.starts_with(' ') || rest.starts_with('#') {
            continue;
        }
        let Some((name, body)) = rest.split_once(':') else {
            continue;
        };
        let fields = flow_fields(body.trim());
        let def = key_def(name).unwrap_or_else(|| panic!("{name} missing from SCHEMA"));
        let f = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let ty = match def.ty {
            Ty::Bool => "bool",
            Ty::Int { .. } => "int",
            Ty::Enum(_) => "enum",
            Ty::Str => "string",
            Ty::List => "list",
            Ty::Map => "map",
        };
        assert_eq!(f("type"), Some(ty), "{name} type");
        assert_eq!(f("scope"), Some(def.scope.label()), "{name} scope");
        let merge = match def.merge {
            Merge::Replace => "replace",
            Merge::Append => "append",
            Merge::SetUnion => "set-union",
            Merge::Deep => "deep-merge",
        };
        assert_eq!(f("merge"), Some(merge), "{name} merge");
        assert_eq!(
            f("security_locked"),
            Some(if def.locked { "true" } else { "false" }),
            "{name} lock"
        );
        if let Ty::Int { min, max } = def.ty {
            assert_eq!(f("min"), Some(min.to_string().as_str()), "{name} min");
            assert_eq!(f("max"), Some(max.to_string().as_str()), "{name} max");
        }
        if let Ty::Enum(values) = def.ty {
            assert_eq!(
                f("enum"),
                Some(format!("[{}]", values.join(", ")).as_str()),
                "{name} enum"
            );
        }
        seen.push(name.to_string());
    }
    for def in SCHEMA {
        assert!(
            seen.iter().any(|s| s == def.key),
            "{} not in the spec",
            def.key
        );
    }
}

/// The `name: value` fields of a one-line YAML flow mapping `{ a: 1, b: [x, y], c: "d, e" }`.
fn flow_fields(body: &str) -> Vec<(String, String)> {
    let inner = body
        .trim()
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .unwrap_or(body);
    let (mut out, mut cur, mut depth, mut quoted) = (Vec::new(), String::new(), 0i32, false);
    let mut push = |cur: &mut String| {
        if let Some((k, v)) = cur.split_once(':') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
        cur.clear();
    };
    let mut prev = ' ';
    for c in inner.chars() {
        match c {
            '"' if prev != '\\' => quoted = !quoted,
            '[' | '{' if !quoted => depth += 1,
            ']' | '}' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                push(&mut cur);
                prev = c;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        prev = c;
    }
    push(&mut cur);
    out
}

/// A three-version history for the migration tests: `editor.tabwidth` became `editor.tabsize` in 1→2
/// and `editor.tab_width` in 2→3 (a chain), the whole `[old]` table went away in 1→2 with one key
/// renamed into `editor` and one removed, and `editor.oldmode` was removed in 2→3.
const STEPS: Migrations = Migrations {
    current: 3,
    steps: &[
        Migration {
            from: 1,
            changes: &[
                Change::Rename {
                    from: "editor.tabwidth",
                    to: "editor.tabsize",
                },
                Change::Rename {
                    from: "old.scroll",
                    to: "editor.scrolloff",
                },
                Change::Manual {
                    key: "old.magic",
                    note: "set editor.ignorecase instead",
                },
            ],
        },
        Migration {
            from: 2,
            changes: &[
                Change::Rename {
                    from: "editor.tabsize",
                    to: "editor.tab_width",
                },
                Change::Manual {
                    key: "editor.oldmode",
                    note: "see docs/design/config-model.md",
                },
            ],
        },
    ],
};

fn parse_with(text: &str, mig: &Migrations) -> (Vec<(&'static str, Value)>, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let entries = parse_layer_in(
        LayerKind::User,
        Path::new("/cfg/user.toml"),
        text,
        &mut diags,
        mig,
    )
    .unwrap();
    (
        entries.into_iter().map(|e| (e.def.key, e.value)).collect(),
        diags,
    )
}

#[test]
fn migration_renames_old_keys_and_warns_about_removed_ones() {
    let (entries, diags) = parse_with(
        "version = 1\n[editor]\ntabwidth = 2\noldmode = true\n[old]\nscroll = 5\nmagic = true\n",
        &STEPS,
    );
    // Auto-conversion: a rename chain applies across both steps; a key moves out of a removed table.
    assert_eq!(
        entries,
        [
            ("editor.tab_width", Value::Int(2)),
            ("editor.scrolloff", Value::Int(5))
        ]
    );
    let migrated: Vec<&str> = diags
        .iter()
        .filter(|d| d.code == "config.migrated")
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        migrated
            .iter()
            .any(|m| m.contains("editor.tabwidth (/cfg/user.toml:3) is now editor.tab_width")),
        "{diags:?}"
    );
    // Manual-warning: the value is not applied, and the note says what to do.
    assert!(migrated
        .iter()
        .any(|m| m.contains("old.magic") && m.contains("set editor.ignorecase instead")));
    // `editor.oldmode` went in 2→3, a step this version-1 file also takes: a manual warning too.
    assert!(migrated
        .iter()
        .any(|m| m.contains("editor.oldmode") && m.contains("manual migration")));
    assert_eq!(migrated.len(), 4, "two renames, two removals: {diags:?}");
    assert_eq!(diags.len(), 4, "nothing else: {diags:?}");
}

#[test]
fn migration_starts_at_the_files_version() {
    // A version-2 file already took the 1→2 step: `old.scroll` is unknown there, `editor.tabsize` renames.
    let (entries, diags) = parse_with("version = 2\neditor.tabsize = 3\nold.scroll = 5\n", &STEPS);
    assert_eq!(entries, [("editor.tab_width", Value::Int(3))]);
    let mut got: Vec<&str> = diags.iter().map(|d| d.code).collect();
    got.sort_unstable();
    assert_eq!(got, ["config.migrated", "config.unknown_key"], "{diags:?}");
    // A current file (no `version`, or `version = 3`) takes no step: old names are unknown keys.
    for text in [
        "editor.tabwidth = 2\n",
        "version = 3\neditor.tabwidth = 2\n",
    ] {
        let (entries, diags) = parse_with(text, &STEPS);
        assert!(entries.is_empty());
        assert_eq!(
            diags.iter().map(|d| d.code).collect::<Vec<_>>(),
            ["config.unknown_key"]
        );
    }
}

#[test]
fn a_newer_or_bad_version_is_read_as_current() {
    let (entries, diags) = parse_with("version = 9\neditor.tab_width = 2\n", &STEPS);
    assert_eq!(entries, [("editor.tab_width", Value::Int(2))]);
    assert_eq!(diags.len(), 1);
    assert!(diags[0].message.contains("newer than this ruse reads (3)"));
    for bad in ["version = 0\n", "version = \"2\"\n"] {
        let (_, diags) = parse_with(bad, &STEPS);
        assert_eq!(
            diags.iter().map(|d| d.code).collect::<Vec<_>>(),
            ["config.invalid_value"],
            "{bad}"
        );
    }
}

/// The shipped table: version 1, no steps, a `version = 1` file loads cleanly, and it mirrors the
/// schema's `version` and `migrations:`.
#[test]
fn the_shipped_migrations_match_the_schema() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/config-schema.yaml");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(&format!("\nversion: {SCHEMA_VERSION}\n")));
    assert!(text.contains("\nmigrations: []\n"), "MIGRATIONS is empty");
    assert!(MIGRATIONS.steps.is_empty());
    let (entries, diags) = parse_with("version = 1\neditor.tab_width = 2\n", &MIGRATIONS);
    assert_eq!(entries.len(), 1);
    assert!(diags.is_empty(), "{diags:?}");
}
