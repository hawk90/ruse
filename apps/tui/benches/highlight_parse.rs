//! Baseline bench #2 (D-042): the per-frame syntax cost — a full tree-sitter re-parse + highlight query,
//! which today runs on every frame because the `Tree` is not kept. Pairs with `edit_apply` (#1) to show
//! which cost dominates at daily-driver sizes.
//!
//! `apps/tui` is a binary (no lib target), so the module is included directly rather than imported; a later
//! frontend-lib split would let benches/tests `use ruse_tui::highlight` instead.

// This bench only exercises `spans()`; the `#[path]`-included module drags fields and a test mod that are
// unused in this target (they are exercised by the real `render`/`cargo test`), tripping dead_code and
// unused_imports here alone. Allow both for the bench shim, never for production code. A later frontend-lib
// split would replace this include with `use ruse_tui::highlight` and drop the allow.
#![allow(dead_code, unused_imports)]

#[path = "../src/highlight.rs"]
mod highlight;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

fn source(lines: usize) -> Vec<u8> {
    "    let value = compute(x, y) + offset; // note\n"
        .repeat(lines)
        .into_bytes()
}

fn bench_parse(c: &mut Criterion) {
    let mut h = highlight::Highlight::rust().expect("rust grammar loads");
    let mut g = c.benchmark_group("highlight_parse");
    for &n in &[100usize, 1_000, 10_000, 100_000] {
        let bytes = source(n);
        g.bench_with_input(BenchmarkId::from_parameter(n), &bytes, |b, bytes| {
            b.iter(|| h.spans(bytes));
        });
    }
    g.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
