//! Baseline bench #1 (D-042): the per-keystroke buffer cost — `EditList::apply_to` (a full `Vec` copy) +
//! `Arc::from` (a second copy) + `rebuild_index` — across document sizes. Pairs with `highlight_parse` (#2)
//! to test the hypothesis that at daily-driver sizes the re-parse (#2), not the buffer copy (#1), dominates.
//!
//! `iter_batched` rebuilds a fresh `Document` per iteration (setup is untimed) because `apply` mutates the
//! document and consumes a revision, so a batch cannot be reused.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use ruse_core::{Document, DocumentId, Edit, EditList, Transaction, TransactionOrigin};

fn source(lines: usize) -> Vec<u8> {
    // A representative code-ish line, repeated. Content is irrelevant to #1 (byte copy + newline scan).
    let line = b"    let value = compute(x, y) + offset;\n";
    let mut v = Vec::with_capacity(lines * line.len());
    for _ in 0..lines {
        v.extend_from_slice(line);
    }
    v
}

fn bench_apply(c: &mut Criterion) {
    let mut g = c.benchmark_group("edit_apply");
    for &n in &[100usize, 1_000, 10_000, 100_000] {
        let bytes = source(n);
        let mid = bytes.len() / 2;
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter_batched(
                || Document::new(DocumentId(1), bytes.clone()),
                |mut doc| {
                    let txn = Transaction::new(
                        doc.revision(),
                        EditList::new(vec![Edit::insert(mid, b"x".to_vec())])
                            .expect("single insert is valid"),
                        TransactionOrigin::UserInput,
                    );
                    doc.apply(txn).expect("in-range insert applies");
                    doc
                },
                BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

criterion_group!(benches, bench_apply);
criterion_main!(benches);
