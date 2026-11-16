use super::*;
use crate::persist;

/// #1194: recover `rec` over the disk text `disk` as the session does, and check it is the disk text plus
/// one change: modified, one `u` back to the clean disk text, and a saved trace that replays from the
/// disk text to `rec` with the same single undo step. The commands recorded, for the caller to inspect.
fn recovers_as_one_change(disk: &[u8], rec: &[u8]) -> Vec<Command> {
    // Both are buffer text, which is line-terminated (RFC-0024): the disk text as read, the recovered
    // text as the offer gives it.
    let (disk, rec) = (
        &persist::encoding::terminated(disk.to_vec())[..],
        &persist::encoding::terminated(rec.to_vec())[..],
    );
    let mut ws = Workspace::new(disk.to_vec());
    ws.focused_doc_mut().mark_saved();
    let mut recorded = Vec::new();
    apply_recovery(&mut ws, rec.to_vec(), &mut recorded);
    assert_eq!(ws.focused().doc.bytes(), rec);
    assert!(ws.focused().doc.is_modified(), "`:q` gives E37");
    ws.focused_doc_mut().undo();
    assert_eq!(
        ws.focused().doc.bytes(),
        disk,
        "one `u` reaches the disk text"
    );
    assert!(!ws.focused().doc.is_modified());
    let text = ruse_core::Trace::record(disk, recorded.clone()).to_text();
    let mut st = ruse_core::Trace::from_text(&text)
        .unwrap()
        .replay(disk)
        .unwrap();
    assert_eq!(st.doc.bytes(), rec, "the trace replays the recovery");
    st.doc.undo();
    assert_eq!(st.doc.bytes(), disk, "and replays it as one undo step");
    recorded
}

/// RFC-0024: in the Emacs profile the empty disk text has no line to read around, and the recovery is
/// still exactly the recovered text (no recursion on an emptied line that profile does not have).
#[test]
fn an_emacs_profile_recovery_of_an_empty_file_is_the_recovered_text() {
    let mut ws = Workspace::new(Vec::new());
    ws.set_caret_gravity(ruse_core::CaretGravity::BetweenChar);
    let mut recorded = Vec::new();
    apply_recovery(&mut ws, b"\xff\n".to_vec(), &mut recorded);
    assert_eq!(ws.focused().doc.bytes(), b"\xff\n");
}

/// #1194: a non-UTF-8 recovery into the emptied buffer (no line to filter) is a read above its one line,
/// then that line goes, as one `ex_begin macro` group. Over a disk text with lines, both texts are
/// line-terminated (RFC-0024), so a filter of every line is the whole change.
#[test]
fn a_recovery_the_line_filter_cannot_carry_is_still_one_change() {
    for rec in [&b"\xff\n"[..], b"a\n\xff\n\n"] {
        let cmds = recovers_as_one_change(b"", rec);
        assert_eq!(cmds.len(), 4, "-> {rec:?}: {cmds:?}");
        assert_eq!(
            cmds[0],
            Command::Ex(ExCmd::Begin(ruse_core::GroupKind::Macro))
        );
        assert_eq!(cmds[3], Command::Ex(ExCmd::End));
    }
    for (disk, rec) in [
        (&b"a\n"[..], &b"\xff"[..]),
        (b"a\nb\n", b"a\nb\xff"),
        (b"a", b"a\xff\n"),
        (b"caf\xe9", b"caf\xe9 \xff\n"),
    ] {
        let cmds = recovers_as_one_change(disk, rec);
        assert_eq!(cmds.len(), 1, "{disk:?} -> {rec:?}: {cmds:?}");
    }
    // A UTF-8 change stays one edit, with no group.
    assert_eq!(recovers_as_one_change(b"", b"new\n").len(), 1);
    assert_eq!(
        recovers_as_one_change(b"caf\xe9\n", b"caf\xe9 au lait\n").len(),
        1
    );
}

mod recovery_fuzz {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// #1194: every recovery, whatever the bytes, is the disk text plus one undoable change that a
        /// saved trace replays.
        #[test]
        fn every_recovery_is_the_disk_text_plus_one_change(
            disk in proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), Just(0xC3u8), Just(0xA9u8), Just(0xFFu8), Just(0x80u8)], 0..16),
            rec in proptest::collection::vec(prop_oneof![Just(b'b'), Just(b'a'), Just(b'\n'), Just(0xC3u8), Just(0xA9u8), Just(0xFFu8)], 0..16),
        ) {
            use crate::persist::encoding::terminated;
            prop_assume!(terminated(disk.clone()) != terminated(rec.clone()));
            recovers_as_one_change(&disk, &rec);
        }
    }
}
