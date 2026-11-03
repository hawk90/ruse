//! A counted insert (`{N}ix<Esc>`, #1123) on the real key path. Each edit copies the buffer, so applying
//! the repetitions one character at a time was quadratic in N; they are now spliced in doubling chunks
//! (capped at 64 KiB, see `run_repeat`), so the cost grows linearly up to the cap.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ruse_tui::headless::Headless;

fn bench_count_insert(c: &mut Criterion) {
    let mut g = c.benchmark_group("count_insert");
    g.sample_size(10);
    for n in [10_000u32, 100_000, 1_000_000] {
        let keys = format!("{n}ix<Esc>");
        g.bench_with_input(BenchmarkId::from_parameter(n), &keys, |b, keys| {
            b.iter(|| {
                let mut h = Headless::vim("a\n");
                h.keys(keys);
                h.cursor()
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench_count_insert);
criterion_main!(benches);
