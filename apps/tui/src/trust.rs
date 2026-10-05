//! Workspace trust (INV-TRUST-1, D-058): the decision that gates any process which would execute code from
//! the workspace — today, language servers (rust-analyzer runs the project's `build.rs` and proc-macros;
//! other servers load project plugins/config). A workspace is **untrusted by default**; it becomes trusted
//! only by an explicit grant from the USER principal, never from anything inside the workspace:
//!
//! - `:trust` — trust the current workspace for this session (handled by the LSP coordinator);
//! - `RUSE_TRUSTED_WORKSPACES` — an OS path list (`:`-separated on Unix) of absolute roots the user trusts;
//!   the workspace is trusted when its root is one of them or inside one. The runtime stand-in for the
//!   `workspace.trusted_roots` config key until a config loader exists (spec/config-schema.yaml).
//!
//! The workspace root is the process working directory (the same root the LSP `rootUri` uses).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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
