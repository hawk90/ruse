//! Bench #2 (D-042): the per-keystroke syntax cost. `highlight_parse` is a FULL reparse (the cost before
//! incremental parsing — measured ~7.5 ms at 1k lines, the dominant per-keystroke cost, which is D-042's
//! trigger). `highlight_incremental` is the cost AFTER F-015 #3: reuse the previous tree and reparse only
//! the edited span. The gap between the two groups is the win.
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
use ruse_core::Revision;

fn source(lines: usize) -> Vec<u8> {
    "    let value = compute(x, y) + offset; // note\n"
        .repeat(lines)
        .into_bytes()
}

/// Full reparse from scratch — a fresh `CachedHighlight` (no prior tree) per call. This is the cost a
/// non-incremental highlighter pays on every edit.
fn bench_full(c: &mut Criterion) {
    let mut g = c.benchmark_group("highlight_parse");
    for &n in &[100usize, 1_000, 10_000, 100_000] {
        let bytes = source(n);
        g.bench_with_input(BenchmarkId::from_parameter(n), &bytes, |b, bytes| {
            b.iter(|| {
                let mut h = highlight::CachedHighlight::rust().expect("rust grammar loads");
                h.spans(Revision(0), bytes, 0..bytes.len()).len()
            });
        });
    }
    g.finish();
}

/// Incremental per-keystroke cost — one 1-byte edit against a primed tree (F-015 #3).
fn bench_incremental(c: &mut Criterion) {
    let mut g = c.benchmark_group("highlight_incremental");
    for &n in &[100usize, 1_000, 10_000, 100_000] {
        let base = source(n);
        g.bench_with_input(BenchmarkId::from_parameter(n), &base, |b, base| {
            let mut h = highlight::CachedHighlight::rust().expect("rust grammar loads");
            h.spans(Revision(0), base, 0..base.len().min(2400)); // prime (viewport ~50 lines)
            let mut edited = base.clone();
            let mut rev: u64 = 1;
            b.iter(|| {
                let mid = edited.len() / 2;
                edited[mid] ^= 1; // a real 1-byte in-place edit
                let out = h
                    .spans(Revision(rev), &edited, 0..edited.len().min(2400))
                    .len();
                rev += 1;
                out
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_full, bench_incremental);
criterion_main!(benches);
