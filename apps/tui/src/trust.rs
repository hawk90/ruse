//! Workspace trust (INV-TRUST-1, D-058): the decision that gates any process which would execute code from
//! the workspace — today, language servers (rust-analyzer runs the project's `build.rs` and proc-macros;
//! other servers load project plugins/config). A workspace is **untrusted by default**; it becomes trusted
//! only by an explicit grant from the USER principal, never from anything inside the workspace:
//!
//! - `:trust` — trust the current workspace for this session (handled by the LSP coordinator);
//! - `workspace.trusted_roots` in the user config, or `RUSE_TRUSTED_WORKSPACES`, an OS path list
//!   (`:`-separated on Unix) of absolute roots the user trusts; the workspace is trusted when its root is
//!   one of them or inside one.
//!
//! The workspace CONFIG layer (`.ruse/config.toml`) has its own, content-bound gate (D-062, #614): `:trust`
//! also writes a user-side [`TrustStore`] record holding a hash of the root's `.ruse/` contents
//! ([`ruse_dir_hash`]), and the layer loads only while the record exists and the hash still matches. A
//! pull or branch switch that changes `.ruse/` makes the layer stale until `:trust` records the new hash;
//! `:untrust` deletes the record. The records live in `${XDG_STATE_HOME:-~/.local/state}/ruse/trust/`,
//! one file per root, named by the SHA-256 of the canonical absolute root path (Helix's workspace trust
//! model), and are never written into the repository.
//!
//! The workspace root is the process working directory (the same root the LSP `rootUri` uses).

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The environment variable listing the user's trusted workspace roots (see the module docs).
pub const TRUSTED_WORKSPACES_ENV: &str = "RUSE_TRUSTED_WORKSPACES";

/// Whether `root` is trusted by the user's `trusted_roots` list: `root` equals a listed root or lies inside
/// one, compared after canonicalization (so a symlinked path cannot dodge or fake a match). Relative or empty
/// entries are ignored — "." would otherwise trust whatever directory ruse happens to start in — and an entry
/// or root that cannot be canonicalized (missing) never matches.
pub fn is_trusted(root: &Path, trusted_roots: impl IntoIterator<Item = PathBuf>) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    trusted_roots
        .into_iter()
        .filter(|r| r.is_absolute())
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| root.starts_with(r))
}

/// [`is_trusted`] against an OS path-list value (the `RUSE_TRUSTED_WORKSPACES` format). `None` (unset) trusts
/// nothing.
pub fn is_trusted_by_list(root: &Path, list: Option<&OsStr>) -> bool {
    list.is_some_and(|v| is_trusted(root, std::env::split_paths(v)))
}

/// The startup trust decision for the workspace at `root`, from the user's `RUSE_TRUSTED_WORKSPACES`.
pub fn workspace_trusted(root: &Path) -> bool {
    is_trusted_by_list(root, std::env::var_os(TRUSTED_WORKSPACES_ENV).as_deref())
}

/// The user-side trust record directory: `${XDG_STATE_HOME:-$HOME/.local/state}/ruse/trust`.
#[must_use]
pub fn trust_dir() -> Option<PathBuf> {
    let var = |n: &str| {
        std::env::var_os(n)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    var("XDG_STATE_HOME")
        .or_else(|| var("HOME").map(|h| h.join(".local").join("state")))
        .map(|s| s.join("ruse").join("trust"))
}

/// The most entries and bytes [`ruse_dir_hash`] reads before it gives up (a `.ruse/` holds a few small
/// config files; this bounds a hostile one).
const MAX_HASH_ENTRIES: usize = 10_000;
const MAX_HASH_BYTES: u64 = 64 << 20;

/// The content hash a trust record binds to: `sha256:` and the hex SHA-256 of every entry under
/// `<root>/.ruse/`, sorted by relative path (`/`-separated bytes). After the tag `ruse-trust-v1\0`, a
/// regular file contributes `F`, its path, `\0`, its length as 8 big-endian bytes and its bytes; a symbolic
/// link (never followed) contributes `L`, its path, `\0`, the link target's length and bytes; directories
/// are walked; anything else is skipped. An absent `.ruse/` is the empty set (D-062), so its hash is the
/// tag's alone.
///
/// # Errors
/// A read error, or a `.ruse/` past [`MAX_HASH_ENTRIES`] / [`MAX_HASH_BYTES`].
pub fn ruse_dir_hash(root: &Path) -> io::Result<String> {
    ruse_dir_snapshot(root).map(|s| s.hash)
}

/// What [`ruse_dir_snapshot`] read: the content hash and the bytes it hashed for `config.toml`.
pub struct RuseSnapshot {
    /// The [`ruse_dir_hash`] of the bytes read.
    pub hash: String,
    /// `.ruse/config.toml` as hashed, when it is a regular file. The loader parses these bytes and never
    /// reads the file again, so a file swapped after the hash cannot load unhashed content.
    pub config: Option<Vec<u8>>,
}

/// Read `<root>/.ruse/` once: its [`ruse_dir_hash`] and the hashed `config.toml` bytes.
///
/// # Errors
/// As [`ruse_dir_hash`].
pub fn ruse_dir_snapshot(root: &Path) -> io::Result<RuseSnapshot> {
    let base = root.join(".ruse");
    let mut entries: Vec<(Vec<u8>, PathBuf, bool)> = Vec::new();
    if std::fs::symlink_metadata(&base).is_ok_and(|m| m.is_dir()) {
        let mut dirs = vec![(base.clone(), Vec::<u8>::new())];
        while let Some((dir, rel)) = dirs.pop() {
            for e in std::fs::read_dir(&dir)? {
                let e = e?;
                let mut name = rel.clone();
                if !name.is_empty() {
                    name.push(b'/');
                }
                name.extend_from_slice(os_bytes(&e.file_name()));
                let ft = e.file_type()?;
                if ft.is_dir() {
                    dirs.push((e.path(), name));
                } else if ft.is_file() || ft.is_symlink() {
                    entries.push((name, e.path(), ft.is_symlink()));
                }
                if entries.len() + dirs.len() > MAX_HASH_ENTRIES {
                    return Err(io::Error::other(".ruse/ has too many entries to hash"));
                }
            }
        }
    }
    entries.sort();
    let mut h = Sha256::new();
    h.update(b"ruse-trust-v1\0");
    let mut total = 0u64;
    let mut config = None;
    for (name, path, link) in &entries {
        let bytes = if *link {
            os_bytes(std::fs::read_link(path)?.as_os_str()).to_vec()
        } else {
            read_bounded(path, MAX_HASH_BYTES - total)?
        };
        total += bytes.len() as u64;
        if total > MAX_HASH_BYTES {
            return Err(io::Error::other(".ruse/ is too large to hash"));
        }
        h.update(if *link { b"L" } else { b"F" });
        h.update(name);
        h.update(b"\0");
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(&bytes);
        if !*link && name.as_slice() == b"config.toml" {
            config = Some(bytes);
        }
    }
    Ok(RuseSnapshot {
        hash: format!("sha256:{}", hex(&h.finalize())),
        config,
    })
}

/// Read `path` without following a symbolic link swapped in after the directory walk, and without
/// holding more than `limit` bytes (a larger file is an error, not a huge allocation).
fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(".ruse/ is too large to hash"));
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn os_bytes(s: &OsStr) -> &[u8] {
    std::os::unix::ffi::OsStrExt::as_bytes(s)
}

#[cfg(not(unix))]
fn os_bytes(s: &OsStr) -> &[u8] {
    s.as_encoded_bytes()
}

/// The user-side trust records: one JSON file per workspace root in [`TrustStore::dir`], named by the hex
/// SHA-256 of the canonical root path, holding `{"root": …, "hash": …}` (the [`ruse_dir_hash`] at grant).
pub struct TrustStore {
    dir: PathBuf,
}

impl TrustStore {
    #[must_use]
    pub fn new(dir: PathBuf) -> TrustStore {
        TrustStore { dir }
    }

    /// Where `root`'s record lives (whether or not it exists).
    #[must_use]
    pub fn record_path(&self, root: &Path) -> PathBuf {
        self.file(root).0
    }

    /// The record file for `root` (canonicalized when it exists).
    fn file(&self, root: &Path) -> (PathBuf, String) {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let name = hex(&Sha256::digest(os_bytes(root.as_os_str())));
        (self.dir.join(name), root.to_string_lossy().into_owned())
    }

    /// The content hash recorded for `root`, if it is trusted.
    #[must_use]
    pub fn lookup(&self, root: &Path) -> Option<String> {
        let (file, root) = self.file(root);
        let text = std::fs::read_to_string(file).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        // The name is a hash of the root; the stored root guards against a mismatched file.
        (v.get("root")?.as_str()? == root).then(|| v.get("hash")?.as_str().map(str::to_string))?
    }

    /// Record `root` as trusted with content hash `hash` (write-new-then-rename; the directory is private
    /// to the user on Unix). Returns the record's path.
    ///
    /// # Errors
    /// The directory or the file cannot be written.
    pub fn record(&self, root: &Path, hash: &str) -> io::Result<PathBuf> {
        let (file, root) = self.file(root);
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let body = serde_json::json!({ "root": root, "hash": hash }).to_string() + "\n";
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &file)?;
        Ok(file)
    }

    /// Delete `root`'s record. Returns whether there was one.
    ///
    /// # Errors
    /// The record exists and cannot be removed.
    pub fn remove(&self, root: &Path) -> io::Result<bool> {
        let (file, _) = self.file(root);
        match std::fs::remove_file(file) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ruse-trust-{}-{name}", std::process::id()));
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        d
    }

    /// The trust decision: unset / empty → untrusted; a listed root trusts itself and everything under it;
    /// a sibling or parent of a listed root is NOT trusted; relative entries are ignored.
    #[test]
    fn trusted_roots_match_root_and_descendants_only() {
        let base = scratch("match");
        let ws = base.join("sub");
        assert!(!is_trusted_by_list(&ws, None), "unset → untrusted");
        assert!(!is_trusted(&ws, Vec::new()), "empty list → untrusted");

        assert!(is_trusted(&ws, vec![ws.clone()]), "the root itself");
        assert!(is_trusted(&ws.join("deeper"), vec![ws.clone()]), "inside");
        assert!(!is_trusted(&base, vec![ws.clone()]), "a parent is not");
        assert!(
            !is_trusted(&ws, vec![base.join("su")]),
            "a string prefix is not a path prefix"
        );
        assert!(
            !is_trusted(&ws, vec![PathBuf::from("."), PathBuf::from("sub")]),
            "relative entries are ignored"
        );

        // The OS path-list form, with an unrelated entry first.
        let list =
            std::env::join_paths([PathBuf::from("/nonexistent-ruse-root"), ws.clone()]).unwrap();
        assert!(is_trusted_by_list(&ws.join("deeper"), Some(&list)));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A symlink cannot fake trust: a link that lives inside a trusted root but points outside it resolves
    /// to the untrusted target.
    #[cfg(unix)]
    #[test]
    fn symlink_into_untrusted_dir_is_not_trusted() {
        let trusted = scratch("trusted");
        let other = scratch("other");
        let link = trusted.join("escape");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert!(!is_trusted(&link, vec![trusted.clone()]));
        let _ = std::fs::remove_dir_all(&trusted);
        let _ = std::fs::remove_dir_all(&other);
    }
}
