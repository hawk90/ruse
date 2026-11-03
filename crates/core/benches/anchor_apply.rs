//! The per-edit anchor cost (INV-ANCHOR, #686, #1122): `:g/pat/normal …` over a big file holds two anchors
//! per marked line, and every edit used to sort the whole store and rebuild the frozen index — `O(A log A)`
//! per edit, so the command went quadratic. The store is now offset-ordered with lazily applied shifts and
//! the index is frozen only for a snapshot, so one edit costs `O(log A)` plus the anchors on its lines.
//!
//! `store/{A}`: one one-byte insert into a store of `A` byte anchors (no text work), repeated on the same
//! store — the number should not grow with `A`. `g_normal/{lines}`: `Document::apply` of one insert into a
//! document holding two anchors per line, as `:g` places them; it includes the `O(n)` buffer copy.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ruse_core::{
    AnchorPolicy, AnchorStore, Bias, BytePos, Document, DocumentId, Edit, EditList, Transaction,
    TransactionOrigin,
};

fn bench_store(c: &mut Criterion) {
    let mut g = c.benchmark_group("anchor_apply/store");
    for &n in &[1_000usize, 100_000, 400_000] {
        let mut store = AnchorStore::new();
        for i in 0..n {
            store.insert(i * 10, Bias::After, AnchorPolicy::Clamp);
        }
        let mid = n * 5;
        let edit = EditList::new(vec![Edit::insert(mid, b"x".to_vec())]).expect("valid");
        store.apply_edits(&edit); // the one re-sort after the inserts, untimed
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| store.apply_edits(&edit));
        });
    }
    g.finish();
}

fn bench_g_normal(c: &mut Criterion) {
    let mut g = c.benchmark_group("anchor_apply/g_normal");
    for &lines in &[1_000usize, 10_000, 100_000] {
        let line = b"    let value = compute(x, y) + offset;\n";
        let mut doc = Document::new(DocumentId(1), line.repeat(lines));
        for l in 0..lines {
            let s = l * line.len();
            doc.create_anchor(BytePos(s), Bias::After, AnchorPolicy::Clamp);
            doc.create_anchor(BytePos(s + line.len()), Bias::Before, AnchorPolicy::Clamp);
        }
        let mid = (lines / 2) * line.len();
        g.bench_with_input(BenchmarkId::from_parameter(lines), &lines, |b, _| {
            b.iter(|| {
                let list = EditList::new(vec![Edit::insert(mid, b"x".to_vec())]).expect("valid");
                let txn = Transaction::new(doc.revision(), list, TransactionOrigin::UserInput);
                doc.apply(txn).expect("in range");
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_store, bench_g_normal);
criterion_main!(benches);
