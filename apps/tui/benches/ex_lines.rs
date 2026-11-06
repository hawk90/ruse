//! Ex line commands through the real key path (#1257): a per-line `:g` runs one Ex command per marked
//! line, so each command must cost O(affected lines), not O(buffer). Cases: `:g/^/m0` (reverse the
//! buffer), `:g/x/d 1`, `:g/x/t$` and `:g/x/j` (half the lines match) on 10k and 100k lines; and one
//! `:m0`, `:t0` and `:d` on the last line of a 100k-line buffer.
//!
//! The session is built outside the timed part; each sample runs the command line once.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use ruse_tui::headless::Headless;

/// `n` lines, alternating `x <i>` and `y <i>`.
fn text(n: usize) -> String {
    let mut s = String::with_capacity(n * 10);
    for i in 0..n {
        s.push_str(if i % 2 == 0 { "x " } else { "y " });
        s.push_str(&i.to_string());
        s.push('\n');
    }
    s
}

fn session(text: &str, at_end: bool) -> Headless {
    let mut h = Headless::vim(text).vanilla_search_case();
    if at_end {
        h.keys("G");
    }
    h
}

fn bench_global(c: &mut Criterion) {
    let mut g = c.benchmark_group("ex_lines_global");
    g.sample_size(10);
    for n in [10_000usize, 100_000] {
        let t = text(n);
        for (name, keys) in [
            ("g_m0", ":g/^/m0<CR>"),
            ("g_d1", ":g/x/d 1<CR>"),
            ("g_t_end", ":g/x/t$<CR>"),
            ("g_j", ":g/x/j<CR>"),
        ] {
            g.bench_function(format!("{name}_{}k", n / 1000), |b| {
                b.iter_batched(
                    || session(&t, false),
                    |mut h| {
                        h.keys(keys);
                        h
                    },
                    BatchSize::LargeInput,
                )
            });
        }
    }
    g.finish();
}

fn bench_single(c: &mut Criterion) {
    let mut g = c.benchmark_group("ex_lines_single");
    g.sample_size(20);
    let t = text(100_000);
    for (name, keys) in [
        ("m0_100k", ":m0<CR>"),
        ("t0_100k", ":t0<CR>"),
        ("d_100k", ":d<CR>"),
    ] {
        g.bench_function(name, |b| {
            b.iter_batched(
                || session(&t, true),
                |mut h| {
                    h.keys(keys);
                    h
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

criterion_group!(benches, bench_single, bench_global);
criterion_main!(benches);
