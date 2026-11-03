//! `*` / `#` / `gd` / `hlsearch` of a `\<word\>` pattern over a large mixed-script buffer (#745). `*`
//! compiles the pattern and scans the whole buffer on every press, so this is that cost. Since #745 the
//! `\<` / `\>` at the pattern's edges are checked after the match with Vim's character classes, and a
//! candidate that fails (the word inside a longer one, or followed by more letters of its script) is
//! skipped: the buffer holds many of those, so the bench covers the retry path, not only clean hits.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ruse_core::{Regex, RegexOptions};

/// `lines` lines of Latin, Hangul and CJK code: `API` as a word, before a Korean particle, inside
/// longer words (`xAPI`, `APIs`), and `호출` before and inside other Hangul.
fn source(lines: usize) -> String {
    "let API를 호출 = xAPI(bar_baz, 한글); // API 문서 APIs 호출함 漢字API 호출\n".repeat(lines)
}

fn bench_word_search(c: &mut Criterion) {
    let mut g = c.benchmark_group("word_search");
    for &n in &[1_000usize, 10_000] {
        let hay = source(n);
        for (name, pat) in [("latin", r"\<API\>"), ("hangul", r"\<호출\>")] {
            g.bench_with_input(BenchmarkId::new(name, n), &hay, |b, hay| {
                b.iter(|| {
                    let re = Regex::compile(pat, RegexOptions::default()).expect("compiles");
                    re.search_all(hay).len()
                })
            });
        }
    }
    g.finish();
}

criterion_group!(benches, bench_word_search);
criterion_main!(benches);
