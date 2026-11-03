//! Property-based tests for the core's load-bearing semantic invariants (testing-and-benchmarks.md §1.2;
//! anti-patterns TEST-5 "no transaction property tests" / TEST-7). Example tests prove *specific* behaviors;
//! these throw thousands of generated inputs at the invariants that must hold for *every* input — the bug
//! class a human reviewer cannot enumerate: a motion that lands mid-codepoint, an edit that corrupts UTF-8,
//! an undo path that doesn't round-trip, an inverse that isn't an inverse.
//!
//! `proptest` is a dev-dependency only; the core crate stays dependency-free at build time.

use proptest::prelude::*;
use ruse_core::{
    anchor::map_byte, apply_command, AnchorId, AnchorPolicy, Bias, BytePos, Command, Document,
    DocumentId, Edit, EditList, EditorState, LineGravity, LineMap, Mode, Motion, Revision,
    SelectKind, Trace, Track, Transaction, TransactionOrigin,
};

/// `n` cases, scaled by `RUSE_PROPTEST_SCALE` for the scheduled deep tier (#920), with the reject budget
/// scaled alike so a filtering test does not abort a long run.
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
        Just(Motion::BigWordFwd),
        Just(Motion::BigWordBack),
        Just(Motion::BigWordEnd),
        Just(Motion::InnerWord),
        Just(Motion::AWord),
        Just(Motion::InnerBigWord),
        Just(Motion::ABigWord),
        Just(Motion::InnerParagraph),
        Just(Motion::AParagraph),
        Just(Motion::InnerSentence),
        Just(Motion::ASentence),
        (any::<char>(), any::<char>(), any::<bool>()).prop_map(|(open, close, around)| {
            Motion::Pair {
                open,
                close,
                around,
            }
        }),
        (any::<char>(), any::<bool>()).prop_map(|(ch, around)| Motion::Quote { ch, around }),
        Just(Motion::Line),
        Just(Motion::GotoLine),
        Just(Motion::LastLine),
        Just(Motion::MatchBracket),
        (any::<char>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
            |(ch, forward, till, repeat)| Motion::FindChar {
                ch,
                forward,
                till,
                repeat
            }
        ),
    ]
}

/// An arbitrary semantic command from the full v0 editing set (including registers and Visual mode).
fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        any::<char>().prop_map(Command::InsertChar),
        Just(Command::InsertNewline),
        Just(Command::DeleteBack),
        (1u32..4).prop_map(Command::DeleteUnder),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Move(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Delete(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Change(n, m)),
        (1u32..4, motion()).prop_map(|(n, m)| Command::Yank(n, m)),
        (any::<bool>(), 1u32..4).prop_map(|(after, count)| Command::Paste {
            after,
            count,
            move_after: false,
        }),
        (0u8..3).prop_map(|k| Command::EnterVisual {
            kind: match k {
                0 => SelectKind::Charwise,
                1 => SelectKind::Linewise,
                _ => SelectKind::Blockwise,
            },
        }),
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
        (1u32..4).prop_map(Command::DeleteUnder),
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
    #![proptest_config(cases(128))]

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
            st.assert_invariants();
            let s = std::str::from_utf8(st.bytes());
            prop_assert!(s.is_ok(), "buffer became invalid UTF-8 after {c:?}");
            let s = s.unwrap();
            prop_assert!(st.cursor().0 <= s.len(), "cursor {} past end {} after {c:?}", st.cursor().0, s.len());
            prop_assert!(
                s.is_char_boundary(st.cursor().0),
                "cursor {} off a char boundary after {c:?}", st.cursor().0
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
        prop_assert_eq!(replayed.cursor().0, direct.cursor().0, "replay reproduces the cursor");

        let reparsed = Trace::from_text(&trace.to_text()).expect("trace text round-trips");
        let replayed2 = reparsed.replay(&init).expect("replay of the reparsed trace succeeds");
        prop_assert_eq!(replayed2.bytes(), direct.bytes(), "serialize→parse→replay is stable");
    }
}

proptest! {
    #![proptest_config(cases(256))]

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
    #![proptest_config(cases(128))]

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
            .map(|(off, b, p)| doc.create_anchor(BytePos(off.min(doc.len())), b, p))
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

/// A buffer of short lines over a tiny alphabet, so the line rule meets empty lines, joins and splits.
fn lined() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop_oneof![Just(b'a'), Just(b'b'), Just(b'\n')], 0..40)
}

/// An edit over a `lined()` buffer, inserting from the same alphabet.
fn lined_edit() -> impl Strategy<Value = Edit> {
    let ins = prop::collection::vec(prop_oneof![Just(b'x'), Just(b'\n')], 0..4);
    (0usize..42, 0usize..6, ins).prop_map(|(p, d, b)| Edit::replace(p, d, b))
}

/// The five ways an anchor can follow an edit.
fn track() -> impl Strategy<Value = Track> {
    prop_oneof![
        Just(Track::Byte(Bias::Before)),
        Just(Track::Byte(Bias::After)),
        Just(Track::Line(LineGravity::Drop)),
        Just(Track::Line(LineGravity::Keep)),
        Just(Track::Line(LineGravity::Cursor)),
    ]
}

proptest! {
    #![proptest_config(cases(256))]

    /// INV-ANCHOR (#686, #758, #941): the store — offset-ordered, with shifts applied lazily per region —
    /// puts every anchor, across several transactions, exactly where the one-anchor rules do (`map_byte`
    /// for a byte anchor, [`LineMap`] for a line anchor), and flags a dropped line anchor.
    #[test]
    fn the_store_moves_each_anchor_by_the_one_anchor_rule(
        init in lined(),
        specs in prop::collection::vec((0usize..45, track()), 0..30),
        txns in prop::collection::vec(prop::collection::vec(lined_edit(), 0..5), 1..5),
    ) {
        let mut doc = Document::new(DocumentId(1), init);
        let mut want: Vec<(AnchorId, usize, Track, bool)> = specs
            .into_iter()
            .map(|(off, t)| {
                let off = off.min(doc.len());
                let id = match t {
                    Track::Byte(b) => doc.create_anchor(BytePos(off), b, AnchorPolicy::Clamp),
                    Track::Line(g) => doc.create_line_anchor(BytePos(off), g),
                };
                (id, off, t, false)
            })
            .collect();
        for raw in txns {
            let Ok(list) = EditList::new(raw) else { continue };
            if list.check_bounds(doc.len()).is_err() {
                continue;
            }
            let old = doc.bytes().to_vec();
            doc.apply(Transaction::new(doc.revision(), list.clone(), TransactionOrigin::UserInput))
                .expect("in range");
            let map = LineMap::new(&old, doc.bytes(), &list);
            for w in &mut want {
                match w.2 {
                    Track::Byte(b) => w.1 = map_byte(&list, w.1, b),
                    Track::Line(g) => match map.map(w.1, g) {
                        Some(p) => w.1 = p,
                        None => {
                            w.1 = map.map(w.1, LineGravity::Keep).expect("Keep never drops");
                            w.3 = true;
                        }
                    },
                }
            }
            for &(id, off, t, gone) in &want {
                let r = doc.resolve_anchor(id).expect("live");
                prop_assert_eq!(r.offset.0, off, "{:?} after {:?}", t, list.edits());
                if matches!(t, Track::Line(_)) {
                    prop_assert_eq!(r.invalidated, gone);
                }
            }
        }
    }

    /// The byte rule keeps the order of anchors of one bias, and at one offset `Before` never lands
    /// past `After` (anchor-store §3, #941): a same-bias range never inverts.
    #[test]
    fn the_byte_rule_is_monotonic_per_bias(
        raw in prop::collection::vec(lined_edit(), 0..6),
        a in 0usize..45,
        b in 0usize..45,
    ) {
        let Ok(list) = EditList::new(raw) else { return Ok(()); };
        let (a, b) = (a.min(b), a.max(b));
        for bias in [Bias::Before, Bias::After] {
            prop_assert!(map_byte(&list, a, bias) <= map_byte(&list, b, bias));
        }
        prop_assert!(map_byte(&list, a, Bias::Before) <= map_byte(&list, a, Bias::After));
    }

    /// The line rule lands in the new text, on a char boundary, and a position on a line no edit
    /// touches keeps its column (nvim's `mark_adjust` moves only line numbers).
    #[test]
    fn the_line_rule_stays_in_bounds_and_keeps_untouched_columns(
        init in text(),
        raw in prop::collection::vec(edit(), 0..4),
        o in 0usize..40,
    ) {
        let Ok(list) = EditList::new(raw) else { return Ok(()); };
        prop_assume!(list.check_bounds(init.len()).is_ok());
        let new = list.apply_to(&init);
        let map = LineMap::new(&init, &new, &list);
        let mut o = o.min(init.len());
        while o < init.len() && o > 0 && (init[o] & 0xC0) == 0x80 {
            o -= 1; // positions rest on char boundaries
        }
        let utf8 = std::str::from_utf8(&new).is_ok();
        for g in [LineGravity::Drop, LineGravity::Keep, LineGravity::Cursor] {
            if let Some(p) = map.map(o, g) {
                prop_assert!(p <= new.len());
                // Edits of raw bytes can break UTF-8; on valid text the result is a char boundary.
                prop_assert!(!utf8 || new.is_empty() || p == new.len() || (new[p] & 0xC0) != 0x80);
            }
        }
        // A line strictly between edits (no edit within a line of it) keeps its column.
        let ls = ruse_core::pos::line_start(&init, o);
        let le = ruse_core::pos::line_end(&init, o);
        let near = list.edits().iter().any(|e| e.pos.0 <= le + 1 && e.end() + 1 >= ls);
        if !near && std::str::from_utf8(&init).is_ok() && std::str::from_utf8(&new).is_ok() {
            let p = map.map(o, LineGravity::Drop).expect("an untouched line is not deleted");
            prop_assert_eq!(p - ruse_core::pos::line_start(&new, p), o - ls);
        }
    }
}

/// Pieces of hard text (#909): wide CJK and Hangul, combining sequences, a ZWJ emoji family, a flag pair,
/// a conjoining-jamo syllable, tabs and line breaks — the clusters a cursor must never split.
const HARD: &[&str] = &[
    "가",
    "漢字",
    "e\u{301}",
    "a\u{301}\u{302}",
    "👨\u{200d}👩\u{200d}👧",
    "🇰🇷",
    "\u{1100}\u{1161}\u{11a8}",
    "\t",
    "\n",
    "a",
    " ",
];

/// Chars to type, each a cluster of its own: a lone flag letter or a leading jamo joins the text after the
/// caret into one cluster around it, and a mark or joiner typed at a line start joins the line above once
/// a `<BS>` removes the break, so those are the starting buffer's business, not the typist's.
const TYPED: &[char] = &['가', '漢', 'e', '👨', '\t', '\n', 'a', ' '];

/// A buffer built from [`HARD`] pieces.
fn hard_text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(HARD), 0..16).prop_map(|ps| ps.concat().into_bytes())
}

/// The extended grapheme cluster boundaries of `s` (UAX #29), as byte offsets including the end.
fn cluster_boundaries(s: &str) -> Vec<usize> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut b: Vec<usize> = s.grapheme_indices(true).map(|(i, _)| i).collect();
    b.push(s.len());
    b
}

/// A motion a key can move the cursor by (a text object selects, it is no bare move: it maps to `h`).
fn bare_motion() -> impl Strategy<Value = Motion> {
    motion().prop_map(|m| {
        let object = matches!(
            m,
            Motion::InnerWord
                | Motion::AWord
                | Motion::InnerBigWord
                | Motion::ABigWord
                | Motion::InnerParagraph
                | Motion::AParagraph
                | Motion::InnerSentence
                | Motion::ASentence
                | Motion::Pair { .. }
                | Motion::Quote { .. }
        );
        if object {
            Motion::Left
        } else {
            m
        }
    })
}

/// Run `cmds` over `init` and check that the cursor rests on a cluster boundary whenever it rests ON a
/// character (Normal mode; in Insert it is an insertion point, and a typed char may join the text after it
/// into one cluster, as a lone flag letter typed before a flag does).
fn rests_on_clusters(init: Vec<u8>, cmds: &[Command]) -> Result<(), TestCaseError> {
    let mut st = EditorState::new(init);
    for c in cmds {
        apply_command(&mut st, c);
        if st.mode() != Mode::Normal {
            continue;
        }
        let s = std::str::from_utf8(st.bytes()).expect("valid UTF-8");
        let b = cluster_boundaries(s);
        prop_assert!(
            b.binary_search(&st.cursor().0).is_ok(),
            "cursor {} inside a cluster of {:?} after {:?}",
            st.cursor().0,
            s,
            c
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(cases(256))]

    /// The cursor never rests inside a user-perceived character (F-002, #909): after any motion, `x`,
    /// undo or `<Esc>` over wide, combining, ZWJ and flag text, it sits on an extended grapheme cluster
    /// boundary. (`e`/`E` landed on a combining mark before #909.)
    #[test]
    fn motions_rest_on_a_cluster_boundary(
        init in hard_text(),
        cmds in prop::collection::vec(prop_oneof![
            6 => (1u32..3, bare_motion()).prop_map(|(n, m)| Command::Move(n, m)),
            1 => (1u32..3).prop_map(Command::DeleteUnder),
            1 => Just(Command::EnterInsert),
            1 => Just(Command::EnterNormal),
            1 => Just(Command::Undo),
        ], 0..30),
    ) {
        rests_on_clusters(init, &cmds)?;
    }

    /// The same over edits that delete or type one char at a time (`X`, Insert `<BS>`, typing): `<BS>`
    /// deleted one code point, half a flag or a base without its marks, before #909.
    #[test]
    fn edits_rest_on_a_cluster_boundary(
        init in hard_text(),
        cmds in prop::collection::vec(prop_oneof![
            4 => (1u32..3, bare_motion()).prop_map(|(n, m)| Command::Move(n, m)),
            1 => (1u32..3).prop_map(Command::DeleteUnder),
            1 => Just(Command::DeleteBack),
            1 => prop::sample::select(TYPED).prop_map(Command::InsertChar),
            1 => Just(Command::EnterInsert),
            1 => Just(Command::EnterNormal),
            1 => Just(Command::Undo),
        ], 0..30),
    ) {
        rests_on_clusters(init, &cmds)?;
    }
}
