//! The shared line math (`crate::pos`) that `row_col` / viewport / cursor placement call once per pane
//! per frame. The audit claimed it is "O(pos), fine at daily sizes" — this measures it instead of
//! asserting it. Worst case is a cursor at the END of the buffer (the scan covers the whole thing).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ruse_core::pos;

fn source(lines: usize) -> Vec<u8> {
    "    let value = compute(x, y) + offset;\n"
        .repeat(lines)
        .into_bytes()
}

fn bench_line_math(c: &mut Criterion) {
    let mut g = c.benchmark_group("line_math");
    for &n in &[1_000usize, 10_000, 100_000] {
        let bytes = source(n);
        let end = bytes.len(); // worst case: last byte → the scan covers the whole buffer
        g.bench_with_input(BenchmarkId::new("line_of_end", n), &bytes, |b, by| {
            b.iter(|| pos::line_of(by, end))
        });
        g.bench_with_input(BenchmarkId::new("line_start_end", n), &bytes, |b, by| {
            b.iter(|| pos::line_start(by, end))
        });
        g.bench_with_input(
            BenchmarkId::new("nth_line_start_last", n),
            &bytes,
            |b, by| b.iter(|| pos::nth_line_start(by, n)),
        );
    }
    g.finish();
}

criterion_group!(benches, bench_line_math);
criterion_main!(benches);
