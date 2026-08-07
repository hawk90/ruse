//! Command-level edit traces — the v0 product pillar (RFC-0012): every edit session is a replayable,
//! shareable list of semantic [`Command`]s. The same format is the test corpus and the `:trace save` /
//! `ruse --replay` feature. Recording at the *command* level (not keystrokes) means a trace survives keymap
//! changes; determinism is a contract — the same initial document + the same trace ⇒ the same final state,
//! which holds automatically because the core is IO-free (all external inputs are Effects).
//!
//! The file format is deliberately dependency-free and human-readable:
//! ```text
//! # ruse-trace v1 doc_hash=<hex>
//! enter_insert
//! insert_char 104
//! enter_normal
//! ```

use crate::command::{Command, CommandParseError};
use crate::editor::{apply_command, EditorState};

/// The trace file format version.
pub const TRACE_FORMAT_VERSION: u32 = 1;

/// A recorded edit session: the initial-document fingerprint plus the commands that were applied.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Trace {
    pub format_version: u32,
    pub doc_hash: u64,
    pub commands: Vec<Command>,
}

/// Why a trace could not be parsed or replayed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TraceError {
    BadHeader(String),
    UnsupportedVersion(u32),
    BadCommand {
        line: usize,
        err: CommandParseError,
    },
    /// The initial document does not match the one the trace was recorded against.
    HashMismatch {
        expected: u64,
        actual: u64,
    },
}

/// A stable, deterministic 64-bit fingerprint of the initial document (FNV-1a — not the randomized std
/// hasher, so a trace's `doc_hash` is reproducible across runs and machines).
#[must_use]
pub fn doc_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Trace {
    /// Record a trace of `commands` applied to a document with `initial` bytes.
    #[must_use]
    pub fn record(initial: &[u8], commands: Vec<Command>) -> Trace {
        Trace {
            format_version: TRACE_FORMAT_VERSION,
            doc_hash: doc_hash(initial),
            commands,
        }
    }

    /// Serialize to the text format.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut s = format!(
            "# ruse-trace v{} doc_hash={:016x}\n",
            self.format_version, self.doc_hash
        );
        for c in &self.commands {
            s.push_str(&c.to_line());
            s.push('\n');
        }
        s
    }

    /// Parse the text format. Blank lines and `#` comments (after the header) are ignored.
    pub fn from_text(text: &str) -> Result<Trace, TraceError> {
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let (version, hash) = parse_header(header)?;
        if version != TRACE_FORMAT_VERSION {
            return Err(TraceError::UnsupportedVersion(version));
        }
        let mut commands = Vec::new();
        for (i, line) in lines.enumerate() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let cmd =
                Command::from_line(t).map_err(|err| TraceError::BadCommand { line: i + 2, err })?;
            commands.push(cmd);
        }
        Ok(Trace {
            format_version: version,
            doc_hash: hash,
            commands,
        })
    }

    /// Replay this trace onto a document with `initial` bytes, returning the final editor state. Fails if
    /// the initial bytes don't match the recorded `doc_hash` (the determinism contract's precondition).
    pub fn replay(&self, initial: &[u8]) -> Result<EditorState, TraceError> {
        let actual = doc_hash(initial);
        if actual != self.doc_hash {
            return Err(TraceError::HashMismatch {
                expected: self.doc_hash,
                actual,
            });
        }
        let mut st = EditorState::new(initial.to_vec());
        for c in &self.commands {
            let _effects = apply_command(&mut st, c);
        }
        Ok(st)
    }
}

fn parse_header(line: &str) -> Result<(u32, u64), TraceError> {
    // `# ruse-trace v1 doc_hash=<hex>`
    let bad = || TraceError::BadHeader(line.to_string());
    let mut version = None;
    let mut hash = None;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix('v') {
            version = v.parse().ok();
        } else if let Some(h) = tok.strip_prefix("doc_hash=") {
            hash = u64::from_str_radix(h, 16).ok();
        }
    }
    if !line.contains("ruse-trace") {
        return Err(bad());
    }
    Ok((version.ok_or_else(bad)?, hash.ok_or_else(bad)?))
}
