//! The revision-cached line index (D-042 win D) vs the O(buffer) `pos` scan it replaces on the per-frame
//! hot path. `line_math` measured the old scan at 0.6–2.1 ms on a 100k-line buffer; this shows the cached
//! lookups are O(log n)/O(1), and the rebuild (once per EDIT, not per frame) is the only O(n) cost left.

// The line index lives in the `ruse-tui` library, so this bench imports it rather than re-compiling the
// module via `#[path]` — the frontend-lib split made `ruse_tui::line_index` reachable.
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ruse_core::Revision;
use ruse_tui::line_index::LineIndex;

fn source(lines: usize) -> Vec<u8> {
    "    let value = compute(x, y) + offset;\n"
        .repeat(lines)
        .into_bytes()
}

fn bench_line_index(c: &mut Criterion) {
    let mut g = c.benchmark_group("line_index");
    for &n in &[1_000usize, 10_000, 100_000] {
        let bytes = source(n);
        let mut idx = LineIndex::default();
        idx.refresh(Revision(0), &bytes);
        let end = bytes.len();

        // Per-frame lookups against a warm index (revision unchanged): O(log n) / O(1).
        g.bench_with_input(BenchmarkId::new("line_of_end", n), &bytes, |b, _| {
            b.iter(|| idx.line_of(end))
        });
        g.bench_with_input(
            BenchmarkId::new("nth_line_start_last", n),
            &bytes,
            |b, _| b.iter(|| idx.nth_line_start(n)),
        );
        // The rebuild cost, paid ONCE per edit (a new revision), not per frame.
        g.bench_with_input(BenchmarkId::new("refresh", n), &bytes, |b, bytes| {
            let mut i = LineIndex::default();
            let mut rev: u64 = 0;
            b.iter(|| {
                rev += 1;
                i.refresh(Revision(rev), bytes);
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench_line_index);
criterion_main!(benches);
