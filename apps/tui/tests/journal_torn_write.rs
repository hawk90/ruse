//! Contract test `journal-torn-write` (`spec/contracts/persistence-format.yaml`, RFC-0023, D-064): a journal
//! cut anywhere (a crash or power loss mid-write) still offers the text of its last intact record, never
//! garbage and never an error; and the crash path's next write cuts the torn tail before appending, so the
//! journal replays again. Runs over a real file system, in a state home of its own.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use ruse_core::{BytePos, Document, DocumentId, Edit, EditList, Transaction, TransactionOrigin};
use ruse_tui::persist::encoding::FileFormat;
use ruse_tui::persist::journal::{self, Journals, Writer};
use ruse_tui::persist::rj01;
use ruse_tui::persist::Recovery;

fn scratch() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "ruse-journal-torn-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn insert(d: &mut Document, at: usize, s: &str) {
    let edits = EditList::new(vec![Edit::insert(BytePos(at), s)]).unwrap();
    d.apply(Transaction::new(
        d.revision(),
        edits,
        TransactionOrigin::UserInput,
    ))
    .unwrap();
    d.sync_undo();
}

/// A journal written by the real writer in two syncs: "disk\n" → "1disk\n" (first sync) → "21disk\n"
/// (second). Returns the journal's path, its bytes, and where the second sync's records begin.
fn two_syncs(j: &Journals, file: &std::path::Path) -> (PathBuf, Vec<u8>, usize) {
    let mut d = Document::new(DocumentId(1), b"disk\n".to_vec());
    d.mark_saved();
    let mut w = Writer::new(j, file, FileFormat::default());
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    insert(&mut d, 0, "1");
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    w.flush(&d).unwrap();
    let first = std::fs::metadata(w.journal()).unwrap().len() as usize;
    insert(&mut d, 0, "2");
    w.absorb(&mut d, FileFormat::default(), Instant::now());
    w.flush(&d).unwrap();
    let raw = std::fs::read(w.journal()).unwrap();
    (w.journal().to_path_buf(), raw, first)
}

/// Cut the second sync (its Edit and its Marker) at every byte: the offer is the first sync's text, and
/// only the whole journal gives the second.
#[test]
fn a_journal_cut_at_every_byte_of_its_last_sync_offers_the_previous_text() {
    let dir = scratch();
    let file = dir.join("f.txt");
    std::fs::write(&file, b"disk\n").unwrap();
    let j = Journals::in_state_home(&dir.join("state"));
    let (path, raw, first) = two_syncs(&j, &file);
    for cut in first..=raw.len() {
        std::fs::write(&path, &raw[..cut]).unwrap();
        let r = rj01::replay(&raw[..cut], None);
        let want: &[u8] = if r.last_good > first {
            b"21disk\n" // the Edit landed whole; only its Marker is torn
        } else {
            b"1disk\n"
        };
        match j.offer(Some(&file), b"disk\n") {
            Recovery::Available(a) => {
                assert_eq!(a.recovered, want, "cut at {cut}");
                assert_eq!(a.lost_tail, cut != r.last_good, "cut at {cut}");
            }
            other => panic!("cut at {cut}: {other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cut anywhere in the whole journal, including the Header and the Checkpoint: the offer is one of the
/// texts the journal held, or nothing; never an error, never other bytes.
#[test]
fn a_journal_cut_anywhere_offers_a_real_text_or_nothing() {
    let dir = scratch();
    let file = dir.join("f.txt");
    std::fs::write(&file, b"disk\n").unwrap();
    let j = Journals::in_state_home(&dir.join("state"));
    let (path, raw, _) = two_syncs(&j, &file);
    for cut in 0..raw.len() {
        std::fs::write(&path, &raw[..cut]).unwrap();
        match j.offer(Some(&file), b"disk\n") {
            Recovery::None => {}
            Recovery::Available(a) => assert!(
                a.recovered == b"1disk\n" || a.recovered == b"21disk\n",
                "cut at {cut}: {:?}",
                a.recovered
            ),
            other => panic!("cut at {cut}: {other:?}"),
        }
    }
    // Zeros after an intact journal (power loss) are a torn tail, not a record.
    let mut zeros = raw.clone();
    zeros.extend(vec![0u8; 4096]);
    std::fs::write(&path, &zeros).unwrap();
    match j.offer(Some(&file), b"disk\n") {
        Recovery::Available(a) => assert_eq!(a.recovered, b"21disk\n"),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// After a torn write, the crash path truncates to the last intact record before it appends, so the new
/// Checkpoint is read and the bytes the replay stopped at are gone.
#[test]
fn the_crash_path_truncates_a_torn_tail_before_it_appends() {
    let dir = scratch();
    let file = dir.join("f.txt");
    std::fs::write(&file, b"disk\n").unwrap();
    let j = Journals::in_state_home(&dir.join("state"));
    let (path, raw, first) = two_syncs(&j, &file);
    let torn = &raw[..first + 7];
    std::fs::write(&path, torn).unwrap();
    journal::rescue(
        &path,
        Some(&journal::canonical(&file)),
        FileFormat::default(),
        b"rescued\n",
        0,
        0,
    )
    .unwrap();
    let after = std::fs::read(&path).unwrap();
    assert_eq!(&after[..first], &raw[..first], "the intact prefix is kept");
    let r = rj01::replay(&after, None);
    assert!(!r.lost_tail);
    assert_eq!(r.text.as_deref(), Some(&b"rescued\n"[..]));
    let _ = std::fs::remove_dir_all(&dir);
}
