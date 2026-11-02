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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every verb the trace codec knows, scraped from `Command::from_line`'s match arms so new verbs are
    /// fuzzed without editing this list.
    fn verbs() -> Vec<String> {
        include_str!("command.rs")
            .lines()
            .filter_map(|l| {
                let rest = l.trim_start().strip_prefix('"')?;
                let (verb, tail) = rest.split_once('"')?;
                let ok = tail.trim_start().starts_with("=>")
                    && !verb.is_empty()
                    && verb.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
                ok.then(|| verb.to_string())
            })
            .collect()
    }

    /// Every motion token, scraped from the body of `motion_token`.
    fn motions() -> Vec<String> {
        let src = include_str!("command.rs");
        let start = src.find("fn motion_token").unwrap();
        let end = src[start..].find("fn motion_from_token").unwrap() + start;
        src[start..end]
            .lines()
            .filter_map(|l| {
                let (_, rhs) = l.split_once("=> \"")?;
                Some(rhs.split('"').next()?.to_string())
            })
            .collect()
    }

    /// A hostile argument: huge, negative, empty, non-numeric, non-ASCII or control-laden.
    fn hostile_arg() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            Just("0".to_string()),
            Just("-1".to_string()),
            Just("4294967295".to_string()),
            Just("18446744073709551616".to_string()),
            Just("1114112".to_string()),
            Just("55296".to_string()),
            "[0-9]{1,3}",
            "[ -~]{0,12}",
            any::<String>().prop_map(|s| s.chars().take(12).collect()),
        ]
    }

    /// #907: a trace line with a maximal count (`emacs_kill_word 4294967295`, `change 4294967295 word_fwd`)
    /// replays in bounded time. Motion steps stopped only after `count` iterations, so such a line spun for
    /// minutes on a four-byte buffer; they now stop at the first step that does not move.
    #[test]
    fn maximal_counts_replay_in_bounded_time() {
        let mut args: Vec<String> = ["4294967295", "4294967295 4294967295"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for m in motions() {
            for pre in ["", "left ", "right "] {
                args.push(format!("{pre}4294967295 {m}"));
                args.push(format!("{pre}{m} 4294967295"));
            }
        }
        let started = std::time::Instant::now();
        let mut slow = Vec::new();
        for v in verbs() {
            for a in &args {
                let line = format!("{v} {a}");
                if let Ok(c) = Command::from_line(&line) {
                    let t = std::time::Instant::now();
                    let _ = Trace::record(b"ab\nc d", vec![c]).replay(b"ab\nc d");
                    if t.elapsed() > std::time::Duration::from_millis(500) {
                        slow.push(format!("{line:?}: {:?}", t.elapsed()));
                    }
                }
            }
        }
        assert!(slow.is_empty(), "slow replays: {slow:#?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
    }

    #[test]
    fn verb_scrape_finds_the_codec() {
        let v = verbs();
        assert!(v.len() > 50, "verb scrape found only {}", v.len());
        assert!(v.iter().any(|x| x == "insert_char"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Arbitrary text never panics the trace parser: it is a trace or a typed error.
        #[test]
        fn from_text_never_panics_on_arbitrary_text(s in any::<String>()) {
            let _ = Trace::from_text(&s);
            let _ = Trace::from_text(&format!("# ruse-trace v1 doc_hash=0\n{s}"));
        }

        /// Arbitrary bytes, lossily decoded the way a reader of a corrupt file would see them.
        #[test]
        fn from_text_never_panics_on_arbitrary_bytes(b in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = Trace::from_text(&String::from_utf8_lossy(&b));
        }

        /// Every known verb with a hostile argument parses or errors; whatever parses re-serializes to a
        /// line that parses back to the same command, and survives replay on a small document.
        #[test]
        fn known_verbs_with_hostile_args_never_panic(
            picks in proptest::collection::vec((any::<proptest::sample::Index>(), hostile_arg()), 1..8),
            initial in "[a-c \n]{0,24}",
        ) {
            let verbs = verbs();
            let mut text = format!("# ruse-trace v1 doc_hash={:016x}\n", doc_hash(initial.as_bytes()));
            for (i, arg) in &picks {
                text.push_str(&format!("{} {}\n", verbs[i.index(verbs.len())], arg));
            }
            // Parse line by line so one bad line does not hide the rest.
            let mut cmds = Vec::new();
            for line in text.lines().skip(1) {
                if let Ok(c) = Command::from_line(line) {
                    let again = Command::from_line(&c.to_line());
                    prop_assert_eq!(again.as_ref(), Ok(&c), "line {:?}", line);
                    cmds.push(c);
                }
            }
            let _ = Trace::from_text(&text);
            let _ = Trace::record(initial.as_bytes(), cmds).replay(initial.as_bytes());
        }
    }
}
