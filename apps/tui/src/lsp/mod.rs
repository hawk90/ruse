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

pub use client::LspClient;
pub use model::{counts, Diag};

use std::path::Path;
use std::process::Command;

/// The language server for a file extension: `(server key, launch command, LSP languageId)`. The key dedups
/// spawns so one process serves every buffer of that language (acceptance: no duplicate process per server).
/// Slice 1 ships Rust only; the map grows (and becomes config-driven) in later slices.
pub fn server_for_ext(ext: &str) -> Option<(&'static str, Command, &'static str)> {
    match ext {
        "rs" => Some(("rust-analyzer", Command::new("rust-analyzer"), "rust")),
        _ => None,
    }
}

/// A `file://` URI for an absolute path. Slice 1 does no percent-encoding — language servers accept a raw
/// absolute path, and the same formatting is used for both `didOpen` and matching `publishDiagnostics` back.
pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}
