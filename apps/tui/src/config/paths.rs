//! Where the config layers and the trust records live, and how a message shows a layer's path.

use std::path::{Path, PathBuf};

/// Where the layers and the trust records live.
#[derive(Clone, Debug, Default)]
pub struct ConfigPaths {
    pub machine: Option<PathBuf>,
    pub user: Option<PathBuf>,
    /// The workspace root (the working directory); its layer is `<root>/.ruse/config.toml`.
    pub workspace_root: Option<PathBuf>,
    /// The user-side trust record directory (`${XDG_STATE_HOME:-~/.local/state}/ruse/trust`).
    pub trust_dir: Option<PathBuf>,
}

impl ConfigPaths {
    /// The paths for a session started in `cwd`, from the environment: `XDG_CONFIG_HOME` / `HOME` for
    /// the user layer, `XDG_STATE_HOME` / `HOME` for the trust records. There is no environment override
    /// of the machine layer: it is the administrator's, and pins keys above the user.
    #[must_use]
    pub fn from_env(cwd: &Path) -> ConfigPaths {
        let var = |n: &str| {
            std::env::var_os(n)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME");
        let config_home =
            var("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
        ConfigPaths {
            machine: Some(PathBuf::from("/etc/ruse/config.toml")),
            user: config_home.map(|c| c.join("ruse").join("config.toml")),
            workspace_root: Some(cwd.to_path_buf()),
            trust_dir: crate::trust::trust_dir(),
        }
    }

    /// The workspace layer's file.
    #[must_use]
    pub fn workspace_file(&self) -> Option<PathBuf> {
        self.workspace_root
            .as_ref()
            .map(|r| r.join(".ruse").join("config.toml"))
    }
}

/// `path` as a message shows it: relative to the working directory when inside it (`.ruse/config.toml`),
/// else with the home directory as `~`, else absolute.
#[must_use]
pub fn short_path(path: &Path) -> String {
    if let Some(rel) = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
    {
        return rel.display().to_string();
    }
    if let Some(rel) = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf))
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}
