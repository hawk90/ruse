//! Journals written by earlier builds still replay (#1318, INV-ADDITIVE). The fixtures under
//! `tests/fixtures/journal/` are real journals: each was left by the `ruse` binary of the commit that wrote
//! that schema version, driven on a PTY through one session and SIGKILLed after its idle sync (the README
//! there says how). A newer major version is refused without touching the journal.

use super::*;
use crate::persist::journal::Journals;
use crate::persist::Recovery;
use ruse_core::undo::MonotonicSeq;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/journal/{name}.rj",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Every record of `raw`, with the `schema_ver` of each.
fn records(raw: &[u8]) -> Vec<(u16, Record)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let ver = u16::from_le_bytes([raw[at + 4], raw[at + 5]]);
        let (r, next) = read_record(raw, at).unwrap_or_else(|s| panic!("record at {at}: {s:?}"));
        out.push((ver, r));
        at = next;
    }
    out
}

/// `raw` with every record's `schema_ver` set to `ver` (each header CRC recomputed).
fn with_version(raw: &[u8], ver: u16) -> Vec<u8> {
    let mut j = raw.to_vec();
    let mut at = 0;
    while at < j.len() {
        let len = le32(&j[at + 8..at + 12]) as usize;
        j[at + 4..at + 6].copy_from_slice(&ver.to_le_bytes());
        let crc = crc32c(&j[at..at + 12]);
        j[at + 12..at + 16].copy_from_slice(&crc.to_le_bytes());
        at += HEAD + len + TRAIL;
    }
    j
}

fn apply(text: &mut Vec<u8>, list: &EditList) {
    for e in list.edits().iter().rev() {
        text.splice(e.pos.0..e.pos.0 + e.del, e.ins.iter().copied());
    }
}

/// The session every fixture recorded, on `doc.txt` = `alpha\r\nbeta\r\n`:
/// `Ihello <Esc>`, `:w`, `jA again<Esc>`, `oworld<Esc>`, `u`, `:set ff=unix`, then 6 s idle and SIGKILL.
/// The `:w` restarts the journal from a Checkpoint of the saved text.
const SAVED: &[u8] = b"hello alpha\nbeta\n";
const RECOVERED: &[u8] = b"hello alpha\nbeta again\n";

/// What every fixture holds, whatever its version: the text, the edits with their undo data, and the
/// undo-tree moves of the `u`.
fn check_session(name: &str, raw: &[u8], want_ver: u16, want_eol: Eol) {
    let recs = records(raw);
    assert!(
        recs.iter().all(|(v, _)| *v == want_ver),
        "{name}: every record is schema {want_ver:#06x}"
    );

    let r = replay(raw, None);
    assert_eq!(r.unreplayable, None, "{name}");
    assert!(!r.lost_tail, "{name}: nothing dropped");
    assert_eq!(
        r.last_good,
        raw.len(),
        "{name}: the next append goes at the end"
    );
    assert_eq!(r.text.as_deref(), Some(RECOVERED), "{name}");
    assert_eq!(r.changes, 18, "{name}: 6 + 6 typed, 6 undone");
    assert!(r.last_marker_ms.is_some(), "{name}: synced before the kill");
    assert_eq!(r.saved, None, "{name}");
    let h = r.header.as_ref().expect("a Header");
    assert_eq!(h.owner.host, b"fixture-host", "{name}");
    assert!(h.path.ends_with(b"/ruse-rj-fixture/doc.txt"), "{name}");
    assert_eq!(h.base, Base::Checkpoint, "{name}");
    assert_eq!(
        h.format.eol,
        Eol::Crlf,
        "{name}: the file was opened as dos"
    );
    let fmt = r.format.expect("a format");
    assert_eq!(fmt.eol, want_eol, "{name}");
    assert!(
        fmt.final_newline && !fmt.bom && !fmt.eof && !fmt.binary,
        "{name}"
    );
    assert_eq!(fmt.encoding, FileEncoding::default(), "{name}");

    // The base, then the edits in order: each is a user edit, its inverse undoes it exactly, and their
    // sequence numbers and revisions run on without a gap.
    let Record::Checkpoint { node, text, .. } = &recs[1].1 else {
        panic!("{name}: a Checkpoint follows the Header");
    };
    assert_eq!(text.as_slice(), SAVED, "{name}");
    let base_node = *node;
    let edits: Vec<&EditRecord> = recs
        .iter()
        .filter_map(|(_, r)| match r {
            Record::Edit(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(edits.len(), 18, "{name}");
    let mut cur = text.clone();
    let mut history = vec![cur.clone()];
    for (i, e) in edits.iter().enumerate() {
        assert_eq!(e.origin, TransactionOrigin::UserInput, "{name} edit {i}");
        assert_eq!(e.result_revision, e.base_revision + 1, "{name} edit {i}");
        if i > 0 {
            assert_eq!(e.meta.seq, edits[i - 1].meta.seq + 1, "{name} edit {i}");
            assert_eq!(
                e.base_revision,
                edits[i - 1].result_revision,
                "{name} edit {i}"
            );
        }
        let before = cur.clone();
        apply(&mut cur, &e.forward);
        let mut undone = cur.clone();
        apply(&mut undone, &e.inverse);
        assert_eq!(
            undone, before,
            "{name} edit {i}: the inverse undoes the edit"
        );
        history.push(cur.clone());
    }
    assert_eq!(cur, RECOVERED, "{name}");

    // 12 fresh nodes (` again`, then the new line `world`), each a child of the one before.
    for (i, e) in edits[..12].iter().enumerate() {
        let node = base_node + 1 + i as u64;
        assert_eq!(
            e.kind,
            ChangeKind::Fresh {
                node: MonotonicSeq(node),
                parent: MonotonicSeq(node - 1),
            },
            "{name} edit {i}"
        );
    }
    assert_eq!(history[12], b"hello alpha\nbeta again\nworld\n", "{name}");
    // The `u` undoes the `oworld` insert: one Navigate per node, back to the node before it.
    for (i, e) in edits[12..].iter().enumerate() {
        assert_eq!(
            e.kind,
            ChangeKind::Navigate {
                to_node: MonotonicSeq(base_node + 11 - i as u64),
            },
            "{name} undo {i}"
        );
    }
    assert_eq!(
        history[18], history[6],
        "{name}: back to the text before the `o`"
    );
}

#[test]
fn a_schema_1_0_journal_replays() {
    let raw = fixture("v1_0");
    // 1.0 did not journal a `:set ff` (no Meta record), so the Header's dos format stands.
    check_session("v1_0", &raw, 1 << 8, Eol::Crlf);
    assert!(!records(&raw)
        .iter()
        .any(|(_, r)| matches!(r, Record::Meta(_))));
}

#[test]
fn a_schema_1_1_journal_replays() {
    let raw = fixture("v1_1");
    check_session("v1_1", &raw, 1 << 8 | 1, Eol::Lf);
}

#[test]
fn a_schema_1_2_journal_replays() {
    let raw = fixture("v1_2");
    check_session("v1_2", &raw, SCHEMA, Eol::Lf);
}

/// The open-time offer recovers each old journal against the file it was written for.
#[test]
fn the_offer_recovers_each_old_journal() {
    for name in ["v1_0", "v1_1", "v1_2"] {
        let dir = crate::TestDir::new(&format!("rj01-compat-{name}"));
        let journals = Journals::in_state_home(&dir.join("state"));
        journals.ensure_dir().unwrap();
        let doc = dir.join("doc.txt");
        std::fs::write(&doc, b"hello alpha\r\nbeta\r\n").unwrap();
        let journal = journals.path_for(&doc);
        std::fs::write(&journal, fixture(name)).unwrap();
        match journals.offer(Some(&doc), SAVED) {
            Recovery::Available(a) => {
                assert_eq!(a.recovered, RECOVERED, "{name}");
                assert_eq!(a.changes, Some(18), "{name}");
                assert!(!a.lost_tail && !a.legacy, "{name}");
                assert_eq!(a.journal, journal, "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// A journal of a newer major version is refused: no text, a note saying so, and the file left as it is
/// for the build that wrote it. A newer minor still replays (its appended fields are skipped).
#[test]
fn a_future_schema_version_is_refused_cleanly() {
    let raw = fixture("v1_2");

    let newer_minor = with_version(&raw, SCHEMA + 1);
    let r = replay(&newer_minor, None);
    assert_eq!(r.unreplayable, None);
    assert_eq!(r.text.as_deref(), Some(RECOVERED));
    assert!(!r.lost_tail);

    let newer_major = with_version(&raw, 2 << 8);
    let r = replay(&newer_major, None);
    assert_eq!(r.unreplayable, Some(Unreplayable::NewerMajor(2)));
    assert_eq!(r.text, None);
    assert_eq!(r.header, None);
    assert_eq!(r.changes, 0);

    let dir = crate::TestDir::new("rj01-compat-future");
    let journals = Journals::in_state_home(&dir.join("state"));
    journals.ensure_dir().unwrap();
    let doc = dir.join("doc.txt");
    std::fs::write(&doc, b"hello alpha\r\nbeta\r\n").unwrap();
    let journal = journals.path_for(&doc);
    std::fs::write(&journal, &newer_major).unwrap();
    match journals.offer(Some(&doc), SAVED) {
        Recovery::Unreplayable { reason } => {
            assert!(reason.contains("newer ruse (format 2.x)"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        newer_major,
        "the journal is kept as it was"
    );
}
