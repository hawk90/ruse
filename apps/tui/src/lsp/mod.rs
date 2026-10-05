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

use std::path::{Path, PathBuf};
use std::process::Command;

use percent_encoding::{percent_decode_str, percent_encode, AsciiSet, NON_ALPHANUMERIC};

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

/// Bytes a `file://` URI path must percent-encode (RFC 3986 §3.3): everything except `unreserved`
/// (ALPHA / DIGIT / `-._~`), `sub-delims` (`!$&'()*+,;=`), `:`, `@`, and the `/` segment separator. So
/// space, `%`, `#`, `?`, `[`, `]`, controls and every non-ASCII byte (UTF-8, byte-wise) are encoded.
const URI_PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/');

/// A `file://` URI for an absolute path (RFC 3986 / RFC 8089): `file://` + the percent-encoded path, so a
/// path with a space, `%`, `#`, `?` or non-ASCII characters round-trips through a language server intact.
/// On Windows, `C:\a\b` becomes `file:///C:/a/b`. Inverse: [`uri_to_path`].
pub fn path_to_uri(path: &Path) -> String {
    #[cfg(unix)]
    let (lead, raw): (&str, std::borrow::Cow<'_, [u8]>) = {
        use std::os::unix::ffi::OsStrExt;
        ("", path.as_os_str().as_bytes().into())
    };
    #[cfg(not(unix))]
    let (lead, raw): (&str, std::borrow::Cow<'_, [u8]>) = {
        let s = path.to_string_lossy().replace('\\', "/");
        let lead = if s.starts_with('/') { "" } else { "/" }; // `C:/x` → `/C:/x`
        (lead, s.into_bytes().into())
    };
    format!("file://{lead}{}", percent_encode(&raw, URI_PATH))
}

/// The filesystem path a `file://` URI names — the inverse of [`path_to_uri`], also accepting what servers
/// send: an optional `localhost` authority and any percent-encoding (decoded byte-wise, so a non-UTF-8 Unix
/// filename survives). A string without the `file://` scheme is taken as a plain path (lenient fallback).
pub fn uri_to_path(uri: &str) -> PathBuf {
    let Some(rest) = uri.strip_prefix("file://") else {
        return PathBuf::from(uri);
    };
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes: Vec<u8> = percent_decode_str(rest).collect();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        let s = String::from_utf8_lossy(&bytes);
        // `/C:/x` → `C:/x` (a drive letter after the leading slash).
        let b = s.as_bytes();
        let s = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            &s[1..]
        } else {
            &s[..]
        };
        PathBuf::from(s)
    }
}

#[cfg(test)]
mod tests {
    use super::{path_to_uri, server_for_ext, uri_to_path};
    use std::path::Path;

    /// Regression: `path_to_uri` percent-encodes per RFC 3986 (previously it pasted the raw path, so a
    /// space / `%` / `#` / non-ASCII path produced an invalid URI that servers mis-parsed — `#` even cut the
    /// path into a fragment). Safe path characters stay literal.
    #[cfg(unix)]
    #[test]
    fn path_to_uri_percent_encodes_reserved_and_non_ascii() {
        let cases = [
            (
                "/home/u/proj/src/main.rs",
                "file:///home/u/proj/src/main.rs",
            ),
            ("/tmp/my dir/a b.rs", "file:///tmp/my%20dir/a%20b.rs"),
            ("/tmp/100%/x#1?.rs", "file:///tmp/100%25/x%231%3F.rs"),
            ("/tmp/한글/é.rs", "file:///tmp/%ED%95%9C%EA%B8%80/%C3%A9.rs"),
            ("/tmp/a[1]{2}.rs", "file:///tmp/a%5B1%5D%7B2%7D.rs"),
            ("/tmp/k=v,x@y:z~_-.rs", "file:///tmp/k=v,x@y:z~_-.rs"),
        ];
        for (path, uri) in cases {
            assert_eq!(path_to_uri(Path::new(path)), uri, "{path}");
            assert_eq!(uri_to_path(uri), Path::new(path), "round trip {uri}");
        }
    }

    /// `uri_to_path` also accepts the shapes servers send: a `localhost` authority, lowercase hex escapes,
    /// and a bare path (lenient fallback). A non-UTF-8 Unix filename survives the round trip.
    #[cfg(unix)]
    #[test]
    fn uri_to_path_accepts_server_shapes() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(uri_to_path("file://localhost/a%20b"), Path::new("/a b"));
        assert_eq!(uri_to_path("file:///x%c3%a9"), Path::new("/xé"));
        assert_eq!(uri_to_path("/plain/path"), Path::new("/plain/path"));
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe"));
        assert_eq!(path_to_uri(odd), "file:///tmp/%FF%FE");
        assert_eq!(uri_to_path(&path_to_uri(odd)), odd);
    }

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
