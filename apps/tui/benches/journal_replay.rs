//! The open-time recovery scan (RFC-0023 §2, `spec/contracts/persistence-format.yaml`
//! `recovery_scan_p95_ms: 500`): read a journal from disk and replay it to the recovered text, as
//! `Journals::offer` does before the prompt. Cases: a 1 MiB document with 10,000 one-character edits (a
//! long typing session), and a 33 MiB document journaled against its disk text (RFC-0023 §5: the scan
//! hashes the disk text) with 1,000 edits.

use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion};
use ruse_core::undo::MonotonicSeq;
use ruse_core::{
    BytePos, ChangeEntry, ChangeKind, Edit, EditList, GroupHint, Revision, TransactionOrigin,
};
use ruse_tui::persist::encoding::FileFormat;
use ruse_tui::persist::journal::{self, Journals};
use ruse_tui::persist::rj01::{self, Base, Header};
use ruse_tui::persist::Recovery;

/// A journal over `disk` (a Checkpoint, or the disk base when `large`) followed by `edits` single-byte
/// inserts at the front, each with its inverse; and the text it replays to.
fn journal(doc: &Path, disk: &[u8], edits: u64, large: bool) -> Vec<u8> {
    let mut j = Vec::new();
    let base = if large {
        Base::Disk {
            len: disk.len() as u64,
            sha256: rj01::sha256(disk),
        }
    } else {
        Base::Checkpoint
    };
    let header = Header {
        owner: journal::owner().clone(),
        path: journal::path_bytes(&journal::canonical(doc)),
        format: FileFormat::default(),
        created_ms: 0,
        base,
    };
    rj01::encode_header(&mut j, &header).unwrap();
    if !large {
        rj01::encode_checkpoint(&mut j, 0, 0, disk).unwrap();
    }
    for n in 0..edits {
        let e = ChangeEntry {
            base_revision: Revision(n),
            result_revision: Revision(n + 1),
            origin: TransactionOrigin::UserInput,
            group_hint: GroupHint::Continue,
            forward: EditList::new(vec![Edit::insert(BytePos(0), "x")]).unwrap(),
            inverse: EditList::new(vec![Edit::delete(BytePos(0), 1)]).unwrap(),
            kind: ChangeKind::Fresh {
                node: MonotonicSeq(n + 1),
                parent: MonotonicSeq(n),
            },
        };
        let meta = rj01::EditMeta {
            seq: n + 1,
            timestamp_ms: n,
        };
        rj01::encode_edit(&mut j, meta, &e).unwrap();
        if n % 200 == 199 {
            rj01::encode_marker(&mut j, n).unwrap();
        }
    }
    j
}

fn setup(tag: &str, disk_len: usize, edits: u64, large: bool) -> (Journals, PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("ruse-bench-journal-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let doc = dir.join("doc.txt");
    let disk: Vec<u8> = b"some text on a line\n"
        .iter()
        .copied()
        .cycle()
        .take(disk_len)
        .collect();
    std::fs::write(&doc, &disk).unwrap();
    let j = Journals::in_state_home(&dir.join("state"));
    j.ensure_dir().unwrap();
    std::fs::write(j.path_for(&doc), journal(&doc, &disk, edits, large)).unwrap();
    (j, doc, disk)
}

fn bench_recovery_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("journal_replay");
    g.sample_size(20);
    for (name, disk_len, edits, large) in [
        ("checkpoint_1MiB_10000_edits", 1 << 20, 10_000, false),
        ("disk_base_33MiB_1000_edits", 33 << 20, 1_000, true),
    ] {
        let (j, doc, disk) = setup(name, disk_len, edits, large);
        g.bench_function(name, |b| {
            b.iter(|| match j.offer(Some(&doc), &disk) {
                Recovery::Available(a) => {
                    assert_eq!(a.recovered.len(), disk.len() + edits as usize)
                }
                other => panic!("{other:?}"),
            })
        });
        let _ = std::fs::remove_dir_all(doc.parent().unwrap());
    }
    g.finish();
}

criterion_group!(benches, bench_recovery_scan);
criterion_main!(benches);
