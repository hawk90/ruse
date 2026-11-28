//! Terminal grid scrolling (#1739): 10k lines of `y\r\n` fed to a 50x200 grid, so every line after the
//! first screenful scrolls the whole screen and pushes a row to scrollback. A per-cell row copy made this
//! about 25 µs a scrolled line here; rotating row handles makes a scroll O(rows), not O(cells).
//! The other cases scroll a DECSTBM region (no scrollback) and reverse-index at the top margin.

#[cfg(unix)]
mod unix {
    use criterion::{criterion_group, Criterion};
    use ruse_tui::term_grid::{ChunkParser, Grid};

    const ROWS: u16 = 50;
    const COLS: u16 = 200;
    const LINES: usize = 10_000;

    fn run(prefix: &[u8], body: &[u8]) -> Grid {
        let mut g = Grid::new(ROWS, COLS);
        let mut p = ChunkParser::new();
        p.advance(&mut g, prefix);
        p.advance(&mut g, body);
        g
    }

    fn bench_scroll(c: &mut Criterion) {
        let mut g = c.benchmark_group("term_scroll");
        g.sample_size(10);
        let lines = b"y\r\n".repeat(LINES);
        g.bench_function("full_screen_10k_lines", |b| b.iter(|| run(b"", &lines)));
        // Region rows 2..=49: LF at the bottom margin scrolls only the region, and nothing goes to scrollback.
        g.bench_function("region_10k_lines", |b| {
            b.iter(|| run(b"\x1b[2;49r\x1b[49;1H", &lines))
        });
        // RI at the top margin scrolls the screen down.
        let ri = b"\x1bM".repeat(LINES);
        g.bench_function("reverse_index_10k", |b| b.iter(|| run(b"", &ri)));
        g.finish();
    }

    criterion_group!(benches, bench_scroll);
}

#[cfg(unix)]
criterion::criterion_main!(unix::benches);

#[cfg(not(unix))]
fn main() {}
