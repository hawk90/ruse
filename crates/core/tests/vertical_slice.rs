//! Headless golden test for the first vertical slice: Document → Transaction → Undo → Snapshot with the
//! anchor store. Each test maps to an acceptance condition of F-001 (transactional editing), F-002
//! (document & coordinate model), or F-005 (undo/redo with logical grouping) in `spec/PRD.yaml`, proving
//! the kernel design end-to-end in code — the "prove the design in code" step, not a UI.

use ruse_core::{
    pos, AnchorPolicy, Bias, Document, DocumentId, Edit, EditList, Revision, Transaction,
    TransactionOrigin, TxnError,
};

/// Build a document with `text` already "on disk" (marked saved at open).
fn opened(text: &str) -> Document {
    let mut d = Document::new(DocumentId(1), text.as_bytes().to_vec());
    d.mark_saved();
    d
}

/// A single-edit transaction against the document's current revision.
fn txn(doc: &Document, edit: Edit, origin: TransactionOrigin) -> Transaction {
    Transaction::new(doc.revision(), EditList::new(vec![edit]).unwrap(), origin)
}

// ---------------------------------------------------------------------------------------------------
// F-001 — Transactional editing
// ---------------------------------------------------------------------------------------------------

#[test]
fn f001_apply_strictly_increases_revision_and_records_origin() {
    let mut doc = opened("abc");
    assert_eq!(doc.revision(), Revision::ZERO);

    let r1 = doc
        .apply(txn(
            &doc,
            Edit::insert(3, b"d".to_vec()),
            TransactionOrigin::UserInput,
        ))
        .unwrap();
    let r2 = doc
        .apply(txn(
            &doc,
            Edit::insert(4, b"e".to_vec()),
            TransactionOrigin::Lsp,
        ))
        .unwrap();

    assert_eq!(doc.as_str(), Some("abcde"));
    assert!(
        r1 > Revision::ZERO && r2 > r1,
        "revision is strictly monotonic (INV-TXN)"
    );
    // The undo node records the revision it produced (persistence §2 result_revision).
    assert_eq!(doc.history().current_result_revision(), doc.revision());
    // Every change records an origin (INV-ORIGIN).
    let origins: Vec<_> = doc
        .history()
        .creation_order()
        .iter()
        .map(|(_, o)| *o)
        .collect();
    assert_eq!(
        origins,
        vec![
            None,
            Some(TransactionOrigin::UserInput),
            Some(TransactionOrigin::Lsp)
        ]
    );
}

#[test]
fn f001_apply_is_atomic_stale_base_revision_is_refused() {
    let mut doc = opened("hello");
    // A transaction built against ZERO after the document already advanced is stale.
    let stale_base = doc.revision();
    doc.apply(txn(
        &doc,
        Edit::insert(5, b"!".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();

    let stale = Transaction::new(
        stale_base,
        EditList::new(vec![Edit::insert(0, b"X".to_vec())]).unwrap(),
        TransactionOrigin::UserInput,
    );
    let before = doc.bytes().to_vec();
    let err = doc.apply(stale).unwrap_err();
    assert!(matches!(err, TxnError::StaleBaseRevision { .. }));
    assert_eq!(
        doc.bytes(),
        &before[..],
        "a refused transaction leaves the document untouched"
    );
}

#[test]
fn f001_apply_is_atomic_out_of_range_is_refused() {
    let mut doc = opened("hi");
    let bad = txn(&doc, Edit::delete(1, 100), TransactionOrigin::UserInput);
    let err = doc.apply(bad).unwrap_err();
    assert!(matches!(err, TxnError::OutOfRange { .. }));
    assert_eq!(doc.as_str(), Some("hi"), "no partial-apply state");
    assert_eq!(
        doc.revision(),
        Revision::ZERO,
        "a rejected apply does not advance the revision"
    );
}

#[test]
fn f001_undo_restores_byte_identical_document() {
    let mut doc = opened("The quick fox");
    let original = doc.bytes().to_vec();

    doc.apply(txn(
        &doc,
        Edit::replace(4, 5, b"slow".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    assert_eq!(doc.as_str(), Some("The slow fox"));

    doc.undo().unwrap();
    assert_eq!(
        doc.bytes(),
        &original[..],
        "undo restores the byte-identical original (F-001)"
    );

    doc.redo().unwrap();
    assert_eq!(
        doc.as_str(),
        Some("The slow fox"),
        "redo re-applies the change"
    );
}

// ---------------------------------------------------------------------------------------------------
// F-002 — Document & coordinate model
// ---------------------------------------------------------------------------------------------------

#[test]
fn f002_anchor_survives_edits_via_not_raw_offsets() {
    let mut doc = opened("cursor here");
    // A cursor anchored just before "here" (byte 7).
    let cursor = doc.create_anchor(7, Bias::After, AnchorPolicy::Clamp);
    // Insert at the start; a raw offset would now point at the wrong text, an anchor tracks the edit.
    doc.apply(txn(
        &doc,
        Edit::insert(0, b">> ".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    assert_eq!(doc.as_str(), Some(">> cursor here"));
    assert_eq!(
        doc.resolve_anchor(cursor).unwrap().offset.0,
        10,
        "anchor followed the +3 shift"
    );
}

#[test]
fn f002_anchor_gravity_at_exact_edit_point() {
    // Two anchors at the same offset with opposite gravity behave differently on an insertion there.
    let mut doc = opened("ab");
    let left = doc.create_anchor(1, Bias::Before, AnchorPolicy::Clamp);
    let right = doc.create_anchor(1, Bias::After, AnchorPolicy::Clamp);
    doc.apply(txn(
        &doc,
        Edit::insert(1, b"XYZ".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    assert_eq!(doc.as_str(), Some("aXYZb"));
    assert_eq!(
        doc.resolve_anchor(left).unwrap().offset.0,
        1,
        "Before gravity stays left of insertion"
    );
    assert_eq!(
        doc.resolve_anchor(right).unwrap().offset.0,
        4,
        "After gravity rides right of insertion"
    );
}

#[test]
fn f002_byte_and_char_coordinates_are_distinct() {
    // "café" — the é is two UTF-8 bytes, so byte and char positions genuinely differ (INV-POS-TYPED).
    let mut doc = opened("café");
    assert_eq!(doc.len(), 5, "5 bytes");
    let text = doc.as_str().unwrap();
    let end = pos::byte_to_char(text, pos::BytePos(5));
    assert_eq!(end.0, 4, "4 chars — byte count != char count");

    // An anchor tracks BYTES; a wide multi-byte insertion elsewhere shifts it by byte length, and the
    // char coordinate is a correct derivation, never desynced.
    let tail = doc.create_anchor(5, Bias::After, AnchorPolicy::Clamp); // end of document
    doc.apply(txn(
        &doc,
        Edit::insert(0, "🎉".as_bytes().to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    assert_eq!(doc.as_str(), Some("🎉café"));
    let off = doc.resolve_anchor(tail).unwrap().offset.0;
    assert_eq!(off, 9, "tail shifted by the 4-byte emoji");
    assert_eq!(
        pos::byte_to_char(doc.as_str().unwrap(), pos::BytePos(off)).0,
        5,
        "🎉+café = 5 chars"
    );
}

// ---------------------------------------------------------------------------------------------------
// F-005 — Undo/redo with logical grouping (branching history)
// ---------------------------------------------------------------------------------------------------

#[test]
fn f005_new_change_after_undo_branches_without_losing_history() {
    let mut doc = opened("");
    doc.apply(txn(
        &doc,
        Edit::insert(0, b"A".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap(); // node 1
    doc.apply(txn(
        &doc,
        Edit::insert(1, b"B".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap(); // node 2
    assert_eq!(doc.as_str(), Some("AB"));

    doc.undo().unwrap(); // back to node 1 ("A")
    assert_eq!(doc.as_str(), Some("A"));

    // A new change here BRANCHES: node 2 ("AB") is not on the current lineage but must not be lost.
    doc.apply(txn(
        &doc,
        Edit::insert(1, b"C".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap(); // node 3
    assert_eq!(doc.as_str(), Some("AC"));

    // Four nodes exist (root + A + B + C); the branched-away "B" state is retained (INV-UNDO, COM-8).
    assert_eq!(doc.history().node_count(), 4);
    let seqs: Vec<u64> = doc
        .history()
        .creation_order()
        .iter()
        .map(|(s, _)| s.0)
        .collect();
    assert_eq!(
        seqs,
        vec![0, 1, 2, 3],
        "chronological index keeps every state ever reached"
    );
}

// ---------------------------------------------------------------------------------------------------
// INV-QUERY-SNAPSHOT — immutable, revision-stamped snapshots
// ---------------------------------------------------------------------------------------------------

#[test]
fn snapshot_is_immutable_under_later_edits() {
    let mut doc = opened("v1");
    let mark = doc.create_anchor(2, Bias::After, AnchorPolicy::Clamp); // end of "v1"
    let snap = doc.snapshot();
    assert_eq!(snap.revision(), Revision::ZERO);

    // Edit the live document after taking the snapshot.
    doc.apply(txn(
        &doc,
        Edit::insert(0, b"prefix-".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    assert_eq!(doc.as_str(), Some("prefix-v1"));

    // The snapshot still reads the old text and the old anchor position — it is frozen at its revision.
    assert_eq!(snap.as_str(), Some("v1"), "snapshot text is immutable");
    assert_eq!(
        snap.resolve(mark).unwrap().offset.0,
        2,
        "snapshot anchor frozen at rev 0"
    );
    // The live document has moved on.
    assert_eq!(
        doc.resolve_anchor(mark).unwrap().offset.0,
        9,
        "live anchor tracked the edit"
    );
    assert!(doc.snapshot().revision() > snap.revision());
}

// ---------------------------------------------------------------------------------------------------
// Golden scenario — one readable end-to-end transcript (edit / undo / redo / dirty-tracking / snapshot)
// ---------------------------------------------------------------------------------------------------

#[test]
fn golden_scenario_transcript() {
    let mut doc = opened("hello");
    let cursor = doc.create_anchor(5, Bias::After, AnchorPolicy::Clamp);
    let mut log = String::new();
    let step = |doc: &Document, label: &str, log: &mut String| {
        log.push_str(&format!(
            "{label:<14} rev={} text={:?} cursor={} modified={}\n",
            doc.revision().0,
            doc.as_str().unwrap(),
            doc.resolve_anchor(cursor).unwrap().offset.0,
            doc.is_modified(),
        ));
    };

    step(&doc, "open", &mut log);
    doc.apply(txn(
        &doc,
        Edit::insert(5, b", world".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    step(&doc, "type", &mut log);
    doc.apply(txn(
        &doc,
        Edit::replace(0, 5, b"HELLO".to_vec()),
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    step(&doc, "upcase", &mut log);
    doc.undo().unwrap();
    step(&doc, "undo", &mut log);
    doc.undo().unwrap();
    step(&doc, "undo", &mut log);
    doc.redo().unwrap();
    step(&doc, "redo", &mut log);

    // Golden expected transcript. `modified` uses undo-node identity: back at the opened node (via two
    // undos) it is false even though the revision counter kept climbing (persistence §1 edge case).
    let expected = "\
open           rev=0 text=\"hello\" cursor=5 modified=false
type           rev=1 text=\"hello, world\" cursor=12 modified=true
upcase         rev=2 text=\"HELLO, world\" cursor=12 modified=true
undo           rev=3 text=\"hello, world\" cursor=12 modified=true
undo           rev=4 text=\"hello\" cursor=5 modified=false
redo           rev=5 text=\"hello, world\" cursor=12 modified=true
";
    assert_eq!(log, expected);
}
