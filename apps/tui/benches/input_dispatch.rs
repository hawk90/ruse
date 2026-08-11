//! The input-engine dispatch (`feed` → the decomposed feed_impl/feed_insert/feed_replace/feed_base).
//! Called once per KEYSTROKE (human-rate), not per frame — so this only confirms the decomposition is
//! not accidentally expensive, not that it is a hot path.

use criterion::{criterion_group, criterion_main, Criterion};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::Mode;
use ruse_tui::input::InputEngine;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn bench_feed(c: &mut Criterion) {
    // A mixed Normal-grammar sequence: motions, an operator+motion, a count, a mode switch.
    let seq: Vec<KeyEvent> = "2wdwx0jli".chars().map(key).collect();
    c.bench_function("input_feed_sequence", |b| {
        b.iter(|| {
            let mut e = InputEngine::new();
            let mut last = 0usize;
            for k in &seq {
                last ^= format!("{:?}", e.feed(*k, Mode::Normal)).len();
            }
            last
        })
    });
}

criterion_group!(benches, bench_feed);
criterion_main!(benches);
