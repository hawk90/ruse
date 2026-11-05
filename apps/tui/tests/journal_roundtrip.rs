//! Contract test `journal-roundtrip` (`spec/contracts/persistence-format.yaml`, RFC-0023, D-064): what the
//! journal writer puts on disk replays to the document's text, every record carries a checksum and the
//! schema version, and each Edit record's stored inverse takes the text back, record by record, to the base
//! it began from (INV-UNDO). Runs the real writer over a real file system, in a state home of its own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use proptest::prelude::*;
use ruse_core::{BytePos, Document, DocumentId, Edit, EditList, Transaction, TransactionOrigin};
use ruse_tui::persist::encoding::{Eol, FileFormat};
use ruse_tui::persist::journal::{self, Journals, Writer};
use ruse_tui::persist::rj01::{self, Record};
use ruse_tui::persist::Recovery;

fn scratch() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "ruse-journal-rt-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opened(text: &[u8]) -> Document {
    let mut d = Document::new(DocumentId(1), text.to_vec());
    d.mark_saved();
    d
}

#[derive(Clone, Debug)]
enum Op {
    Insert(usize, String),
    Delete(usize, usize),
    Undo,
    Redo,
    Sync,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<usize>(), "[a-c\n]{1,4}").prop_map(|(at, s)| Op::Insert(at, s)),
        2 => (any::<usize>(), 1usize..4).prop_map(|(at, n)| Op::Delete(at, n)),
        1 => Just(Op::Undo),
        1 => Just(Op::Redo),
        1 => Just(Op::Sync),
    ]
}

fn run(d: &mut Document, op: &Op) {
    let len = d.bytes().len();
    let txn = |edits: Vec<Edit>, d: &Document| {
        Transaction::new(
            d.revision(),
            EditList::new(edits).unwrap(),
            TransactionOrigin::UserInput,
        )
    };
    match op {
        Op::Insert(at, s) => {
            let t = txn(vec![Edit::insert(BytePos(at % (len + 1)), s.as_str())], d);
            d.apply(t).unwrap();
        }
        Op::Delete(at, n) if len > 0 => {
            let at = at % len;
            let t = txn(vec![Edit::delete(BytePos(at), (*n).min(len - at))], d);
            d.apply(t).unwrap();
        }
        Op::Delete(..) => {}
        Op::Undo => {
            d.undo();
        }
        Op::Redo => {
            d.redo();
        }
        Op::Sync => {}
    }
    d.sync_undo();
}

/// Walk a journal's Edit records back from `text` with their inverses; the base the stream began from.
fn undo_all(raw: &[u8], text: &[u8]) -> Vec<u8> {
    let mut edits = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let (rec, next) = rj01::read_record(raw, at).expect("an intact journal");
        match rec {
            Record::Edit(e) => edits.push(e),
            Record::Checkpoint { .. } => edits.clear(),
            _ => {}
        }
        at = next;
    }
    let mut t = text.to_vec();
    for e in edits.iter().rev() {
        assert!(e.inverse.check_bounds(t.len()).is_ok());
        t = e.inverse.apply_to(&t);
    }
    t
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Any edit history, synced at any points: the journal replays to the text, and its inverses walk it
    /// back to the text the journal began from (the disk text).
    #[test]
    fn the_journal_replays_forward_and_inverts_back(
        init in "[a-c\n]{0,12}",
        ops in proptest::collection::vec(op(), 1..40),
    ) {
        let dir = scratch();
        let file = dir.join("f.txt");
        let j = Journals::in_state_home(&dir.join("state"));
        let mut d = opened(init.as_bytes());
        let mut w = Writer::new(&j, &file, FileFormat::default());
        w.absorb(&mut d, FileFormat::default(), Instant::now());
        for op in &ops {
            run(&mut d, op);
            w.absorb(&mut d, FileFormat::default(), Instant::now());
            if matches!(op, Op::Sync) {
                w.flush(&d).unwrap();
            }
        }
        w.flush(&d).unwrap();
        if d.is_modified() {
            let raw = journal::read_journal(w.journal()).unwrap();
            let r = rj01::replay(&raw, None);
            prop_assert!(!r.lost_tail);
            prop_assert_eq!(r.text.as_deref(), Some(d.bytes()));
            // The stream began at the disk text, or at a Checkpoint the writer took instead.
            let back = undo_all(&raw, d.bytes());
            let first_checkpoint = (|| {
                let mut at = 0;
                while at < raw.len() {
                    let (rec, next) = rj01::read_record(&raw, at).ok()?;
                    if let Record::Checkpoint { text, .. } = rec {
                        return Some(text);
                    }
                    at = next;
                }
                None
            })();
            prop_assert_eq!(Some(back), first_checkpoint);
        } else {
            prop_assert!(!w.journal().exists(), "a clean buffer has no journal");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Every record is framed with the schema version and two CRC32C checksums, within the contract's
/// 64-byte per-record overhead budget.
#[test]
fn every_record_carries_a_checksum_and_the_schema_version() {
    let dir = scratch();
    let file = dir.join("f.txt");
    let j = Journals::in_state_home(&dir.join("state"));
    let mut d = opened(b"text\n");
    let mut w = Writer::new(&j, &file, FileFormat::default());
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    run(&mut d, &Op::Insert(0, "x".into()));
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    w.flush(&d).unwrap();
    let raw = journal::read_journal(w.journal()).unwrap();
    const { assert!(rj01::HEAD + rj01::TRAIL <= 64, "overhead budget") };
    let mut at = 0;
    let mut kinds = Vec::new();
    while at < raw.len() {
        assert_eq!(&raw[at..at + 4], b"RJ01");
        assert_eq!(u16::from_le_bytes([raw[at + 4], raw[at + 5]]), rj01::SCHEMA);
        assert_eq!(rj01::SCHEMA, 0x0100, "schema 1.0");
        kinds.push(raw[at + 6]);
        let len = u32::from_le_bytes(raw[at + 8..at + 12].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(raw[at + 12..at + 16].try_into().unwrap());
        assert_eq!(crc, rj01::crc32c(&raw[at..at + 12]));
        let payload = &raw[at + 16..at + 16 + len];
        let pcrc = u32::from_le_bytes(raw[at + 16 + len..at + 20 + len].try_into().unwrap());
        assert_eq!(pcrc, rj01::crc32c(payload));
        assert_eq!(&raw[at + 20 + len..at + 24 + len], b"rj01");
        at += 24 + len;
    }
    // Header, Checkpoint (the disk text), the Edit, the sync's Marker.
    assert_eq!(kinds, [1, 2, 3, 5]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Header names the owner, the canonical path and the file format; a format change is a Meta record;
/// the open-time offer gives the recovered text back for the existing prompt.
#[test]
fn the_header_and_meta_records_round_trip_through_the_offer() {
    let dir = scratch();
    let file = dir.join("dos.txt");
    std::fs::write(&file, b"a\r\n").unwrap();
    let j = Journals::in_state_home(&dir.join("state"));
    let crlf = FileFormat {
        eol: Eol::Crlf,
        ..FileFormat::default()
    };
    let mut d = opened(b"a\n");
    let mut w = Writer::new(&j, &file, crlf);
    w.absorb(&mut d, crlf, Instant::now());
    run(&mut d, &Op::Insert(0, "b".into()));
    w.absorb(&mut d, crlf, Instant::now());
    run(&mut d, &Op::Insert(0, "c".into()));
    w.absorb(&mut d, FileFormat::default(), Instant::now()); // `:set ff=unix` before the next change
    w.flush(&d).unwrap();
    let r = rj01::replay(&journal::read_journal(w.journal()).unwrap(), None);
    let h = r.header.unwrap();
    assert_eq!(h.owner.pid, std::process::id());
    assert_eq!(h.path, journal::path_bytes(&journal::canonical(&file)));
    assert_eq!(h.format, crlf);
    assert_eq!(
        r.format,
        Some(FileFormat::default()),
        "the Meta format change"
    );
    match j.offer(Some(&file), b"a\n") {
        Recovery::Available(a) => assert_eq!(a.recovered, b"cba\n"),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// #1196: a file whose name is as long as a name can be is journaled; its journal name fits NAME_MAX.
#[test]
fn a_file_with_a_255_byte_name_is_journaled() {
    let dir = scratch();
    let file = dir.join("n".repeat(255));
    let j = Journals::in_state_home(&dir.join("state"));
    if std::fs::write(&file, b"disk\n").is_err() {
        return; // a file system with a shorter limit
    }
    let journal = j.write_text(Some(&file), b"work\n").unwrap();
    assert!(journal.file_name().unwrap().len() <= 255);
    assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"work\n"[..]));
    assert!(Path::new(&journal).starts_with(j.dir()));
    let _ = std::fs::remove_dir_all(&dir);
}
