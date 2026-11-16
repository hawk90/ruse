//! Live reload: which keys changed, which may change in a running session, and the on-disk stamp
//! that tells when a layer file changed.

use super::{key_def, Config, ConfigPaths, SCHEMA};

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
