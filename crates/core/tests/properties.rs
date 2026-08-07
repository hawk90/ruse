//! Property-based tests for the core's load-bearing semantic invariants (testing-and-benchmarks.md §1.2;
//! anti-patterns TEST-5 "no transaction property tests" / TEST-7). Example tests prove *specific* behaviors;
//! these throw thousands of generated inputs at the invariants that must hold for *every* input — the bug
//! class a human reviewer cannot enumerate: a motion that lands mid-codepoint, an edit that corrupts UTF-8,
//! an undo path that doesn't round-trip, an inverse that isn't an inverse.
//!
//! `proptest` is a dev-dependency only; the core crate stays dependency-free at build time.

use proptest::prelude::*;
use ruse_core::{
    apply_command, AnchorPolicy, Bias, Command, Document, DocumentId, Edit, EditList, EditorState,
    Motion, Revision, Trace, Transaction, TransactionOrigin,
};

/// An arbitrary buffer: any sequence of Unicode scalars (incl. newlines / control chars) as UTF-8 bytes.
fn text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<char>(), 0..30)
        .prop_map(|cs| cs.into_iter().collect::<String>().into_bytes())
}

/// An arbitrary motion.
fn motion() -> impl Strategy<Value = Motion> {
    prop_oneof![
        Just(Motion::Left),
        Just(Motion::Right),
        Just(Motion::Up),
        Just(Motion::Down),
        Just(Motion::LineStart),
        Just(Motion::LineEnd),
        Just(Motion::WordFwd),
        Just(Motion::WordBack),
        Just(Motion::WordEnd),
        Just(Motion::InnerWord),
        Just(Motion::AWord),
        Just(Motion::Line),
        (any::<char>(), any::<bool>(), any::<bool>())
            .prop_map(|(ch, forward, till)| Motion::FindChar { ch, forward, till }),
    ]
}

/// An arbitrary semantic command from the full v0 editing set (including registers and Visual mode).
fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        any::<char>().prop_map(Command::InsertChar),
        Just(Command::InsertNewline),
        Just(Command::DeleteBack),
        Just(Command::DeleteUnder),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Move(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Delete(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Change(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Yank(n, m)),
        any::<bool>().prop_map(|after| Command::Paste { after }),
        any::<bool>().prop_map(|line| Command::EnterVisual { line }),
        Just(Command::DeleteSelection),
        Just(Command::YankSelection),
        Just(Command::ChangeSelection),
        Just(Command::EnterInsert),
        Just(Command::EnterInsertAfter),
        Just(Command::EnterNormal),
        Just(Command::Undo),
        Just(Command::Redo),
    ]
}

/// Editing commands only (no Undo/Redo — those are driven explicitly by the round-trip test).
fn edit_command() -> impl Strategy<Value = Command> {
    prop_oneof![
        any::<char>().prop_map(Command::InsertChar),
        Just(Command::InsertNewline),
        Just(Command::DeleteBack),
        Just(Command::DeleteUnder),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Delete(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Change(n, m)),
        Just(Command::EnterInsert),
        Just(Command::EnterNormal),
    ]
}

/// A raw candidate edit over a ~30-byte buffer; many will be out of range and are filtered by `check_bounds`.
fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (0usize..30, prop::collection::vec(any::<u8>(), 0..4))
            .prop_map(|(p, b)| Edit::insert(p, b)),
        (0usize..30, 0usize..8).prop_map(|(p, d)| Edit::delete(p, d)),
        (
            0usize..30,
            0usize..8,
            prop::collection::vec(any::<u8>(), 0..4)
        )
            .prop_map(|(p, d, b)| Edit::replace(p, d, b)),
    ]
}

/// An arbitrary anchor gravity.
fn bias() -> impl Strategy<Value = Bias> {
    prop_oneof![Just(Bias::Before), Just(Bias::After)]
}

/// An arbitrary span-delete policy.
fn policy() -> impl Strategy<Value = AnchorPolicy> {
    prop_oneof![Just(AnchorPolicy::Clamp), Just(AnchorPolicy::Invalidate)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// The headline safety property: no sequence of commands, from any buffer, ever produces invalid UTF-8
    /// or leaves the cursor off a character boundary or out of bounds. This is the invariant that a
    /// mid-codepoint motion or a bad delete would violate — invisible to example tests.
    #[test]
    fn command_sequences_preserve_utf8_and_cursor_boundary(
        init in text(),
        cmds in prop::collection::vec(command(), 0..60),
    ) {
        let mut st = EditorState::new(init);
        for c in &cmds {
            apply_command(&mut st, c);
            let s = std::str::from_utf8(st.bytes());
            prop_assert!(s.is_ok(), "buffer became invalid UTF-8 after {c:?}");
            let s = s.unwrap();
            prop_assert!(st.cursor() <= s.len(), "cursor {} past end {} after {c:?}", st.cursor(), s.len());
            prop_assert!(
                s.is_char_boundary(st.cursor()),
                "cursor {} off a char boundary after {c:?}", st.cursor()
            );
        }
    }

    /// Full undo returns to the initial buffer; full redo returns to the edited buffer — for any edit
    /// sequence, regardless of how edits coalesce into undo groups (INV-UNDO substrate).
    #[test]
    fn undo_all_then_redo_all_round_trips(
        init in text(),
        cmds in prop::collection::vec(edit_command(), 1..40),
    ) {
        let mut st = EditorState::new(init.clone());
        for c in &cmds {
            apply_command(&mut st, c);
        }
        let edited = st.bytes().to_vec();

        let mut guard = 0;
        while st.doc.can_undo() {
            apply_command(&mut st, &Command::Undo);
            guard += 1;
            prop_assert!(guard < 10_000, "undo did not terminate");
        }
        prop_assert_eq!(st.bytes(), init.as_slice(), "full undo restores the initial buffer");

        guard = 0;
        while st.doc.can_redo() {
            apply_command(&mut st, &Command::Redo);
            guard += 1;
            prop_assert!(guard < 10_000, "redo did not terminate");
        }
        prop_assert_eq!(st.bytes(), edited.as_slice(), "full redo restores the edited buffer");
    }

    /// Replaying a recorded trace on the same initial document reproduces the exact final state reached by
    /// driving the commands directly — and survives a serialize→parse round-trip. Determinism is the trace
    /// pillar's core contract (RFC-0012).
    #[test]
    fn trace_replay_is_deterministic(
        init in text(),
        cmds in prop::collection::vec(command(), 0..40),
    ) {
        let mut direct = EditorState::new(init.clone());
        for c in &cmds {
            apply_command(&mut direct, c);
        }

        let trace = Trace::record(&init, cmds);
        let replayed = trace.replay(&init).expect("replay on the recorded initial doc succeeds");
        prop_assert_eq!(replayed.bytes(), direct.bytes(), "replay reproduces the buffer");
        prop_assert_eq!(replayed.cursor(), direct.cursor(), "replay reproduces the cursor");

        let reparsed = Trace::from_text(&trace.to_text()).expect("trace text round-trips");
        let replayed2 = reparsed.replay(&init).expect("replay of the reparsed trace succeeds");
        prop_assert_eq!(replayed2.bytes(), direct.bytes(), "serialize→parse→replay is stable");
    }
}

proptest! {
    /// A constructed `EditList` is always sorted ascending and pairwise disjoint (touching-at-a-point is
    /// allowed); overlaps are rejected, never silently accepted.
    #[test]
    fn editlist_is_sorted_and_disjoint(raw in prop::collection::vec(edit(), 0..6)) {
        if let Ok(list) = EditList::new(raw) {
            let es = list.edits();
            for w in es.windows(2) {
                prop_assert!(
                    w[0].end() <= w[1].pos.0,
                    "edits overlap or are unsorted: {:?} then {:?}", w[0], w[1]
                );
            }
        }
    }

    /// Applying an edit list and then its inverse restores the original buffer exactly — the substrate every
    /// undo relies on. Holds for any buffer and any in-range edit list.
    #[test]
    fn apply_then_inverse_is_identity(
        buf in prop::collection::vec(any::<u8>(), 0..30),
        raw in prop::collection::vec(edit(), 0..5),
    ) {
        let Ok(list) = EditList::new(raw) else { return Ok(()); };
        prop_assume!(list.check_bounds(buf.len()).is_ok());
        let after = list.apply_to(&buf);
        let inverse = list.inverse(&buf);
        let restored = inverse.apply_to(&after);
        prop_assert_eq!(restored, buf, "inverse(apply(buf)) must equal buf");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Transaction atomicity (INV-TXN / F-001): a transaction applies fully or not at all. A rejected apply
    /// — stale base revision, or an edit out of range — leaves the document's bytes AND revision *exactly*
    /// unchanged (no partial mutation, no revision bump); a successful apply strictly advances the revision.
    #[test]
    fn failed_transaction_leaves_document_untouched(
        init in text(),
        raw in prop::collection::vec(edit(), 0..5),
        wrong_base in any::<bool>(),
    ) {
        let mut doc = Document::new(DocumentId(1), init);
        let Ok(list) = EditList::new(raw) else { return Ok(()); };
        let in_range = list.check_bounds(doc.len()).is_ok();

        // A wrong base is any revision != current; the correct base is the document's own revision.
        let base = if wrong_base {
            Revision(doc.revision().0 + 1)
        } else {
            doc.revision()
        };
        let expected = if !wrong_base && in_range {
            list.apply_to(doc.bytes())
        } else {
            doc.bytes().to_vec()
        };
        let txn = Transaction::new(base, list, TransactionOrigin::UserInput);

        let bytes_before = doc.bytes().to_vec();
        let rev_before = doc.revision();
        match doc.apply(txn) {
            Ok(_) => {
                // Success is only possible with the correct base and in-range edits.
                prop_assert!(!wrong_base && in_range, "apply succeeded on an invalid transaction");
                prop_assert!(doc.revision() > rev_before, "a successful apply must advance the revision");
                prop_assert_eq!(doc.bytes(), expected.as_slice(), "apply produced the wrong buffer");
            }
            Err(_) => {
                // Atomicity: a rejected transaction changes nothing at all.
                prop_assert_eq!(doc.bytes(), bytes_before.as_slice(), "failed apply mutated the buffer");
                prop_assert_eq!(doc.revision(), rev_before, "failed apply advanced the revision");
            }
        }
    }

    /// Anchor-transform safety (INV-ANCHOR): after any in-range edit, every still-live anchor resolves to a
    /// position inside the document — a transformed anchor never points past the end. (The full affinity /
    /// span-delete semantics have their own example tests; this is the totality guarantee over random edits.)
    #[test]
    fn anchors_stay_in_bounds_across_edits(
        init in text(),
        specs in prop::collection::vec((0usize..40, bias(), policy()), 0..6),
        raw in prop::collection::vec(edit(), 0..4),
    ) {
        let mut doc = Document::new(DocumentId(1), init);
        let ids: Vec<_> = specs
            .into_iter()
            .map(|(off, b, p)| doc.create_anchor(off.min(doc.len()), b, p))
            .collect();

        let Ok(list) = EditList::new(raw) else { return Ok(()); };
        prop_assume!(list.check_bounds(doc.len()).is_ok());
        let txn = Transaction::new(doc.revision(), list, TransactionOrigin::UserInput);
        doc.apply(txn).expect("in-range transaction on the current revision applies");

        let len = doc.len();
        for id in ids {
            if let Some(r) = doc.resolve_anchor(id) {
                prop_assert!(
                    r.offset.0 <= len,
                    "anchor resolved to {} past end {}", r.offset.0, len
                );
            }
        }
    }
}
