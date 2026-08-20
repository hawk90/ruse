//! The Language Service host (F-014 slice 1): a local LSP client per server, normalizing `publishDiagnostics`
//! into the byte-range [`Diag`] model the UI reads. Cross-platform (stdio pipes, not a PTY). Slice 1 covers
//! local diagnostics for Rust (`rust-analyzer`); more languages, more requests, merge + remote come later.
//!
//! **Determinism boundary (F-022):** LSP I/O is external and non-deterministic — it never mutates a
//! `Document`, is not recorded as `Command`s, and `--replay` ignores it.

pub mod client;
pub mod codec;
pub mod model;
pub mod protocol;
pub mod snippet;

pub use client::LspClient;
pub use model::{counts, Diag};

use std::path::Path;
use std::process::Command;

/// The language server for a file extension: `(server key, launch command, LSP languageId)`. The key dedups
/// spawns so one process serves every buffer of that language (acceptance: no duplicate process per server) —
/// e.g. `.ts` and `.js` share `typescript-language-server`, `.c`/`.cpp`/`.h` share `clangd`. A missing binary
/// is a silent no-op (`LspClient::spawn` → `None`), so an unavailable server never breaks the editor. This
/// hard-coded map is the seam a config-driven `language-servers` registry replaces later.
pub fn server_for_ext(ext: &str) -> Option<(&'static str, Command, &'static str)> {
    let (key, bin, args, lang): (_, _, &[&str], _) = match ext {
        "rs" => ("rust-analyzer", "rust-analyzer", &[], "rust"),
        "py" | "pyi" => ("pyright", "pyright-langserver", &["--stdio"], "python"),
        "ts" | "tsx" => (
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            "typescript",
        ),
        "js" | "jsx" | "mjs" | "cjs" => (
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            "javascript",
        ),
        "go" => ("gopls", "gopls", &[], "go"),
        "c" => ("clangd", "clangd", &[], "c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "h" => ("clangd", "clangd", &[], "cpp"),
        "lua" => ("lua-language-server", "lua-language-server", &[], "lua"),
        _ => return None,
    };
    let mut cmd = Command::new(bin);
    cmd.args(args);
    Some((key, cmd, lang))
}

/// A `file://` URI for an absolute path. Slice 1 does no percent-encoding — language servers accept a raw
/// absolute path, and the same formatting is used for both `didOpen` and matching `publishDiagnostics` back.
pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::server_for_ext;

    /// Each supported extension maps to the expected `(server key, languageId)`; extensions that share a
    /// server (ts/js → typescript-language-server; c/cpp/h → clangd) reuse the SAME key so one process serves
    /// them all. Unknown extensions have no server.
    #[test]
    fn server_for_ext_maps_languages_and_dedups_by_key() {
        let key_lang = |ext: &str| server_for_ext(ext).map(|(k, _, l)| (k, l));
        assert_eq!(key_lang("rs"), Some(("rust-analyzer", "rust")));
        assert_eq!(key_lang("py"), Some(("pyright", "python")));
        assert_eq!(key_lang("go"), Some(("gopls", "go")));
        // ts and js share one server key but keep distinct languageIds.
        assert_eq!(
            key_lang("ts"),
            Some(("typescript-language-server", "typescript"))
        );
        assert_eq!(
            key_lang("jsx"),
            Some(("typescript-language-server", "javascript"))
        );
        // c and its C++ siblings share `clangd`.
        assert_eq!(key_lang("c"), Some(("clangd", "c")));
        assert_eq!(key_lang("hpp"), Some(("clangd", "cpp")));
        assert_eq!(key_lang("txt"), None);
        assert_eq!(key_lang(""), None);
    }
}
