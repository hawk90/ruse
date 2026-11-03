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
//!
//! Ex edits are recorded too, as resolved [`ExCmd`]s (`ex_*` verbs, RFC-0020): line numbers, patterns and
//! the bytes `:r`/`:!` read are values, so replay needs no Ex parser and does no IO. A trace the editor
//! saves starts with `ex_set` lines holding the editing options the core reads, as they were when the
//! session began, and `ex_begin`/`ex_end` bracket the commands that undo as one step (`:normal`, `:g`'s
//! `normal`, a macro). These are additive verbs: a `v1` trace written before them parses and replays as
//! before.

use crate::command::{Command, CommandParseError};
use crate::editor::{apply_command, EditorOption, EditorState, ExCmd};
use crate::register::Register;

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
    /// An `ex_end` with no open `ex_begin` (at `line`), or a group still open at the end of the trace
    /// (`line` is the last line).
    UnbalancedGroup {
        line: usize,
    },
    /// A record at `line` past a live limit: an `ex_edits` batch with more edits or text than
    /// `apply_edits` takes, or an `ex_register` value past the register size limit (RFC-0020 R6).
    TooLarge {
        line: usize,
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

    /// Record a trace whose replay starts from the editing `options` the session had when it began
    /// (RFC-0020 R4): they are written as `ex_set` lines before the commands, so a replay on a machine
    /// with another config gives the same document. Then the `registers` that held a value when it began
    /// ([`crate::RegisterStore::filled`]), as `ex_register` lines (R6), so a `p` of one replays the same
    /// text. A group still open (a `:trace save` run from inside a `:normal` or a macro) is closed at the
    /// end, so the saved trace parses.
    #[must_use]
    pub fn record_with_options(
        initial: &[u8],
        options: &[EditorOption],
        registers: &[(Option<char>, Register)],
        commands: Vec<Command>,
    ) -> Trace {
        let mut all: Vec<Command> = options
            .iter()
            .map(|&o| Command::Ex(ExCmd::Set(o)))
            .chain(registers.iter().map(|(name, value)| {
                Command::Ex(ExCmd::Register {
                    name: *name,
                    value: value.clone(),
                })
            }))
            .collect();
        let mut depth = 0usize;
        for c in &commands {
            match c {
                Command::Ex(ExCmd::Begin(_)) => depth += 1,
                Command::Ex(ExCmd::End) => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        all.extend(commands);
        all.extend(std::iter::repeat_n(Command::Ex(ExCmd::End), depth));
        Trace::record(initial, all)
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
        let mut depth = 0usize;
        let mut last = 1;
        for (i, line) in lines.enumerate() {
            last = i + 2;
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let cmd = Command::from_line(t).map_err(|err| match err {
                CommandParseError::TooLarge(_) => TraceError::TooLarge { line: i + 2 },
                err => TraceError::BadCommand { line: i + 2, err },
            })?;
            match cmd {
                Command::Ex(ExCmd::Begin(_)) => depth += 1,
                Command::Ex(ExCmd::End) => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or(TraceError::UnbalancedGroup { line: i + 2 })?;
                }
                _ => {}
            }
            commands.push(cmd);
        }
        if depth > 0 {
            return Err(TraceError::UnbalancedGroup { line: last });
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
        // An `ex_begin`/`ex_end` group undoes as one step, as `:normal` and a macro replay do live: the
        // OUTERMOST group holds one undo block open (an inner one folds into it, as the live block does).
        let mut depth = 0usize;
        for c in &self.commands {
            match c {
                Command::Ex(ExCmd::Begin(_)) => {
                    if depth == 0 {
                        st.doc.begin_undo_block();
                    }
                    depth += 1;
                }
                Command::Ex(ExCmd::End) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        st.doc.end_undo_block();
                    }
                }
                _ => {
                    let _effects = apply_command(&mut st, c);
                }
            }
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
            .chain(include_str!("editor/ex_cmd.rs").lines())
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

    /// A `v1` trace written before RFC-0020 (no `ex_*` verbs, no starting options) still parses and
    /// replays as it did (INV-ADDITIVE).
    #[test]
    fn a_trace_from_before_ex_verbs_still_replays() {
        let initial = b"hello\nworld\n";
        let old = format!(
            "# ruse-trace v1 doc_hash={:016x}\nmove_line_end\nenter_insert_after\ninsert_char 33\n\
             enter_normal\n# a comment\n\nmove 1 down\ndelete 1 line\n",
            doc_hash(initial)
        );
        let trace = Trace::from_text(&old).expect("an old trace parses");
        assert_eq!(trace.commands.len(), 6);
        let st = trace.replay(initial).expect("replay");
        assert_eq!(st.bytes(), b"hello!\n");
    }

    #[test]
    fn unbalanced_groups_are_typed_errors() {
        let head = format!("# ruse-trace v1 doc_hash={:016x}\n", doc_hash(b""));
        assert_eq!(
            Trace::from_text(&format!("{head}ex_end\n")),
            Err(TraceError::UnbalancedGroup { line: 2 })
        );
        assert_eq!(
            Trace::from_text(&format!("{head}ex_begin normal\nex_begin global\nex_end\n")),
            Err(TraceError::UnbalancedGroup { line: 4 })
        );
        assert!(Trace::from_text(&format!("{head}ex_begin macro\nex_end\n")).is_ok());
        // Saved from inside a group: the open groups are closed, so the file parses.
        let open = vec![
            Command::Ex(ExCmd::Begin(crate::editor::GroupKind::Macro)),
            Command::Ex(ExCmd::Begin(crate::editor::GroupKind::Normal)),
        ];
        let t = Trace::record_with_options(b"", &[], &[], open);
        assert_eq!(
            Trace::from_text(&t.to_text()).map(|t| t.commands.len()),
            Ok(4)
        );
    }

    /// The commands between `ex_begin` and `ex_end` undo as one step on replay, as `:normal` does live;
    /// without the group they are separate steps.
    #[test]
    fn a_group_replays_as_one_undo_step() {
        let initial = b"a\nb\n";
        let body = |grouped: bool| {
            let mut cmds = Vec::new();
            if grouped {
                cmds.push(Command::Ex(ExCmd::Begin(crate::editor::GroupKind::Normal)));
            }
            for at in [0, 2] {
                cmds.push(Command::Ex(ExCmd::Place(at)));
                cmds.push(Command::AppendLineEnd);
                cmds.push(Command::InsertChar('x'));
                cmds.push(Command::EnterNormal);
                cmds.push(Command::Ex(ExCmd::UndoSync));
            }
            if grouped {
                cmds.push(Command::Ex(ExCmd::End));
            }
            cmds.push(Command::Undo);
            Trace::record(initial, cmds).replay(initial).unwrap()
        };
        assert_eq!(body(true).bytes(), b"a\nb\n");
        assert_eq!(body(false).bytes(), b"ax\nb\n");
    }

    /// A saved trace starts from the session's options, whatever the replaying defaults are.
    #[test]
    fn starting_options_are_replayed_first() {
        let initial = b"x Foo foo\n";
        let search = Command::Search {
            op: crate::command::SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: crate::command::SearchOffset::None,
        };
        let opts = [
            EditorOption::IgnoreCase(true),
            EditorOption::SmartCase(true),
        ];
        let t = Trace::record_with_options(initial, &opts, &[], vec![search.clone()]);
        let t = Trace::from_text(&t.to_text()).unwrap();
        assert_eq!(t.replay(initial).unwrap().cursor(), 2);
        let plain = Trace::record(initial, vec![search]);
        assert_eq!(plain.replay(initial).unwrap().cursor(), 6);
        let t = Trace::record_with_options(initial, &opts, &[], vec![]);
        assert!(t
            .to_text()
            .contains("ex_set ignorecase 1\nex_set smartcase 1\n"));
    }

    /// The registers filled before the trace began are written after the options (RFC-0020 R6), so a put
    /// of one replays the same text.
    #[test]
    fn starting_registers_are_replayed_before_the_commands() {
        let initial = b"a\n";
        let regs = [
            (None, Register::linewise(b"un\n".to_vec())),
            (Some('q'), Register::charwise(b"Q".to_vec())),
        ];
        let cmds = vec![
            Command::Paste {
                count: 1,
                after: true,
                move_after: false,
            },
            Command::SetRegister(Some('q')),
            Command::Paste {
                count: 1,
                after: true,
                move_after: false,
            },
        ];
        let t = Trace::record_with_options(initial, &[], &regs, cmds);
        let text = t.to_text();
        assert!(
            text.contains("ex_register - l 756e0a\nex_register 113 c 51\n"),
            "{text}"
        );
        let st = Trace::from_text(&text).unwrap().replay(initial).unwrap();
        assert_eq!(st.bytes(), b"a\nuQn\n");
    }

    /// A record past a live limit is a typed error, found without decoding its bytes.
    #[test]
    fn records_past_the_live_limits_are_typed_errors() {
        let head = format!("# ruse-trace v1 doc_hash={:016x}\n", doc_hash(b""));
        let many = "0:0:- ".repeat(crate::MAX_APPLY_EDITS + 1);
        assert_eq!(
            Trace::from_text(&format!("{head}ex_edits lsp {many}\n")),
            Err(TraceError::TooLarge { line: 2 })
        );
    }

    #[test]
    fn verb_scrape_finds_the_codec() {
        let v = verbs();
        assert!(v.len() > 50, "verb scrape found only {}", v.len());
        assert!(v.iter().any(|x| x == "insert_char"));
    }

    proptest! {
        #![proptest_config(crate::test_cases(512))]

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
