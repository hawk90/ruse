//! The drainable change log (RFC-0023 §1, D-064): one entry per revision bump on every path, replayable
//! forwards from the start text and backwards through the inverses, bounded with an overflow the writer
//! cannot miss.

use proptest::prelude::*;
use ruse_core::{
    apply_command, BytePos, ChangeDrain, ChangeEntry, ChangeKind, ChangeLogCap, CheckpointReason,
    Command, Document, DocumentId, Edit, EditList, EditorState, GroupHint, MonotonicSeq, Motion,
    Transaction, TransactionOrigin,
};

/// `n` cases, scaled by `RUSE_PROPTEST_SCALE` for the scheduled deep tier (#920).
fn cases(n: u32) -> ProptestConfig {
    let scale = std::env::var("RUSE_PROPTEST_SCALE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    let base = ProptestConfig::default();
    ProptestConfig {
        cases: n.saturating_mul(scale),
        max_global_rejects: base.max_global_rejects.saturating_mul(scale),
        ..base
    }
}

fn changes(d: ChangeDrain) -> Vec<ChangeEntry> {
    match d {
        ChangeDrain::Changes(v) => v,
        ChangeDrain::Checkpoint { reason, .. } => panic!("unexpected checkpoint: {reason:?}"),
    }
}

/// Turn the log on and consume the `Started` checkpoint, checking it carries the current text.
fn start(doc: &mut Document) {
    doc.record_changes(true);
    match doc.drain_changes() {
        ChangeDrain::Checkpoint {
            reason: CheckpointReason::Started,
            text,
            revision,
            node,
        } => {
            assert_eq!(text.as_slice(), doc.bytes());
            assert_eq!(revision, doc.revision());
            assert_eq!(node, doc.history().current_seq());
        }
        other => panic!("turning the log on must start with a checkpoint, got {other:?}"),
    }
}

/// The log's contract over a run from `start` (at `start_rev`) to `doc`: entries chain revision to
/// revision with one per bump, each forward takes its base text to its result and each inverse takes it
/// back, the whole forward replay rebuilds the document and the reversed inverses rebuild `start`, and the
/// last entry names the current undo node.
fn check_log(
    start: &[u8],
    start_rev: ruse_core::Revision,
    log: &[ChangeEntry],
    doc: &Document,
) -> Result<(), TestCaseError> {
    let bumps = doc.revision().0 - start_rev.0;
    prop_assert_eq!(log.len() as u64, bumps, "one entry per revision bump");
    let mut rev = start_rev;
    let mut text = start.to_vec();
    for e in log {
        prop_assert_eq!(e.base_revision, rev, "entries chain");
        prop_assert_eq!(e.result_revision, rev.next(), "one bump per entry");
        rev = e.result_revision;
        prop_assert!(e.forward.check_bounds(text.len()).is_ok());
        let next = e.forward.apply_to(&text);
        prop_assert!(e.inverse.check_bounds(next.len()).is_ok());
        prop_assert_eq!(&e.inverse.apply_to(&next), &text, "inverse undoes forward");
        text = next;
    }
    prop_assert_eq!(
        text.as_slice(),
        doc.bytes(),
        "forward replay rebuilds the text"
    );
    let mut back = doc.bytes().to_vec();
    for e in log.iter().rev() {
        back = e.inverse.apply_to(&back);
    }
    prop_assert_eq!(back.as_slice(), start, "inverse replay rebuilds the start");
    if let Some(last) = log.last() {
        let node = match last.kind {
            ChangeKind::Fresh { node, .. } => node,
            ChangeKind::Navigate { to_node } => to_node,
        };
        prop_assert_eq!(
            node,
            doc.history().current_seq(),
            "the last entry names the current node"
        );
    }
    Ok(())
}

/// One step against the bare Document API.
#[derive(Clone, Debug)]
enum Op {
    Edit {
        pos: usize,
        del: usize,
        ins: Vec<u8>,
        hint: u8,
        origin: bool,
    },
    Empty,
    Sync,
    Undo,
    Redo,
    Older,
    Newer,
    LineUndo(usize),
    Block(bool),
    Drain,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0usize..40, 0usize..6, prop::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), any::<u8>()], 0..5), 0u8..3, any::<bool>())
            .prop_map(|(pos, del, ins, hint, origin)| Op::Edit { pos, del, ins, hint, origin }),
        1 => Just(Op::Empty),
        1 => Just(Op::Sync),
        2 => Just(Op::Undo),
        2 => Just(Op::Redo),
        1 => Just(Op::Older),
        1 => Just(Op::Newer),
        1 => (0usize..40).prop_map(Op::LineUndo),
        1 => any::<bool>().prop_map(Op::Block),
        1 => Just(Op::Drain),
    ]
}

fn run(doc: &mut Document, op: &Op, log: &mut Vec<ChangeEntry>) {
    match op {
        Op::Edit {
            pos,
            del,
            ins,
            hint,
            origin,
        } => {
            let len = doc.len();
            let pos = (*pos).min(len);
            let del = (*del).min(len - pos);
            let list = EditList::new(vec![Edit::replace(BytePos(pos), del, ins.clone())]).unwrap();
            let hint = [
                GroupHint::Continue,
                GroupHint::BreakBefore,
                GroupHint::JoinPrev,
            ][usize::from(*hint)];
            let origin = if *origin {
                TransactionOrigin::UserInput
            } else {
                TransactionOrigin::Lsp
            };
            doc.apply(Transaction::new(doc.revision(), list, origin).with_hint(hint))
                .unwrap();
        }
        Op::Empty => {
            let empty = EditList::new(Vec::new()).unwrap();
            doc.apply(Transaction::new(
                doc.revision(),
                empty,
                TransactionOrigin::UserInput,
            ))
            .unwrap();
        }
        Op::Sync => doc.sync_undo(),
        Op::Undo => {
            doc.undo();
        }
        Op::Redo => {
            doc.redo();
        }
        Op::Older => {
            doc.undo_chronological(true);
        }
        Op::Newer => {
            doc.undo_chronological(false);
        }
        Op::LineUndo(at) => {
            let at = (*at).min(doc.len());
            doc.line_undo(BytePos(at));
        }
        Op::Block(true) => doc.begin_undo_block(),
        Op::Block(false) => doc.end_undo_block(),
        Op::Drain => log.extend(changes(doc.drain_changes())),
    }
}

/// Editor commands, so the log is checked on the real paths too (operators, Insert sessions, `U`, `g-`).
fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        3 => prop_oneof![Just('a'), Just('b'), Just(' '), Just('é')].prop_map(Command::InsertChar),
        1 => Just(Command::InsertNewline),
        1 => Just(Command::DeleteBack),
        1 => (1u32..3).prop_map(Command::DeleteUnder),
        1 => (1u32..3).prop_map(|n| Command::Delete(n, Motion::WordFwd)),
        1 => (1u32..3).prop_map(|n| Command::Change(n, Motion::LineEnd)),
        1 => (1u32..3).prop_map(|n| Command::Move(n, Motion::WordBack)),
        1 => (1u32..3).prop_map(|n| Command::Move(n, Motion::Down)),
        1 => Just(Command::EnterInsert),
        1 => Just(Command::EnterNormal),
        2 => Just(Command::Undo),
        2 => Just(Command::Redo),
        1 => Just(Command::UndoOlder),
        1 => Just(Command::UndoNewer),
        1 => Just(Command::LineUndo),
    ]
}

proptest! {
    #![proptest_config(cases(256))]

    /// Any run of edits, empty transactions, undos, redos, chronological moves, `U` and undo blocks, with
    /// drains anywhere in it: the drained entries replay forwards to the document and backwards to the
    /// start.
    #[test]
    fn the_drained_log_replays_both_ways(
        init in prop::collection::vec(prop_oneof![Just(b'x'), Just(b'\n')], 0..30),
        ops in prop::collection::vec(op(), 0..60),
    ) {
        let mut doc = Document::new(DocumentId(1), init.clone());
        start(&mut doc);
        let start_rev = doc.revision();
        let mut log = Vec::new();
        for op in &ops {
            run(&mut doc, op, &mut log);
            doc.assert_invariants();
        }
        log.extend(changes(doc.drain_changes()));
        check_log(&init, start_rev, &log, &doc)?;
        prop_assert!(changes(doc.drain_changes()).is_empty(), "a drain clears the log");
    }

    /// The same through editor commands.
    #[test]
    fn the_log_of_editor_commands_replays_both_ways(
        init in "[ab \n]{0,20}",
        cmds in prop::collection::vec(command(), 0..50),
    ) {
        let init = init.into_bytes();
        let mut st = EditorState::new(init.clone());
        start(&mut st.doc);
        let start_rev = st.doc.revision();
        let mut log = Vec::new();
        for c in &cmds {
            apply_command(&mut st, c);
            log.extend(changes(st.doc.drain_changes()));
        }
        check_log(&init, start_rev, &log, &st.doc)?;
    }
}

fn insert(doc: &mut Document, at: usize, s: &[u8]) {
    let list = EditList::new(vec![Edit::insert(BytePos(at), s.to_vec())]).unwrap();
    doc.apply(
        Transaction::new(doc.revision(), list, TransactionOrigin::UserInput)
            .with_hint(GroupHint::BreakBefore),
    )
    .unwrap();
}

#[test]
fn an_overflow_drops_the_entries_and_the_next_drain_checkpoints() {
    let mut doc = Document::new(DocumentId(1), b"abc".to_vec());
    doc.set_change_log_cap(ChangeLogCap {
        entries: 3,
        bytes: 1 << 20,
    });
    start(&mut doc);
    for _ in 0..3 {
        insert(&mut doc, 0, b"x");
    }
    assert_eq!(
        changes(doc.drain_changes()).len(),
        3,
        "at the cap is not over it"
    );
    for _ in 0..5 {
        insert(&mut doc, 0, b"y");
    }
    doc.undo();
    match doc.drain_changes() {
        ChangeDrain::Checkpoint {
            reason: CheckpointReason::Overflow,
            text,
            revision,
            node,
        } => {
            assert_eq!(text.as_slice(), b"yyyyxxxabc");
            assert_eq!(revision, doc.revision());
            assert_eq!(node, doc.history().current_seq());
        }
        other => panic!("expected an overflow checkpoint, got {other:?}"),
    }
    // Recording goes on from the checkpoint.
    insert(&mut doc, 0, b"z");
    let after = changes(doc.drain_changes());
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].forward.apply_to(b"yyyyxxxabc"), doc.bytes());
}

#[test]
fn a_change_larger_than_the_byte_cap_overflows() {
    let mut doc = Document::new(DocumentId(1), Vec::new());
    doc.set_change_log_cap(ChangeLogCap {
        entries: 100,
        bytes: 64,
    });
    start(&mut doc);
    insert(&mut doc, 0, &[b'q'; 65]);
    assert!(matches!(
        doc.drain_changes(),
        ChangeDrain::Checkpoint {
            reason: CheckpointReason::Overflow,
            ..
        }
    ));
}

#[test]
fn the_log_is_off_by_default_and_says_so() {
    let mut doc = Document::new(DocumentId(1), b"abc".to_vec());
    assert!(!doc.is_recording_changes());
    insert(&mut doc, 0, b"x");
    assert!(matches!(
        doc.drain_changes(),
        ChangeDrain::Checkpoint {
            reason: CheckpointReason::NotRecording,
            ..
        }
    ));
    start(&mut doc);
    doc.record_changes(false);
    insert(&mut doc, 0, b"x");
    assert!(matches!(
        doc.drain_changes(),
        ChangeDrain::Checkpoint {
            reason: CheckpointReason::NotRecording,
            ..
        }
    ));
}

#[test]
fn entries_name_the_undo_nodes() {
    // root(0) -> A(1); u; root -> B(2); g- -> A; C-r has nothing; u -> root.
    let mut doc = Document::new(DocumentId(1), b"abc".to_vec());
    start(&mut doc);
    insert(&mut doc, 0, b"A");
    doc.undo();
    insert(&mut doc, 0, b"B");
    doc.undo_chronological(true);
    let log = changes(doc.drain_changes());
    let kinds: Vec<ChangeKind> = log.iter().map(|e| e.kind).collect();
    let (root, a, b) = (MonotonicSeq(0), MonotonicSeq(1), MonotonicSeq(2));
    assert_eq!(
        kinds,
        vec![
            ChangeKind::Fresh {
                node: a,
                parent: root
            },
            ChangeKind::Navigate { to_node: root },
            ChangeKind::Fresh {
                node: b,
                parent: root
            },
            // g- from B to A crosses the root: B's inverse lands on the root, A's forward on A.
            ChangeKind::Navigate { to_node: root },
            ChangeKind::Navigate { to_node: a },
        ]
    );
    assert_eq!(doc.bytes(), b"Aabc");
}
