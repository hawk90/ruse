//! #1109: commands on a 1 MB single-line non-ASCII buffer (minified JSON with Unicode, CJK prose without
//! breaks). Each command measures the wanted screen column (`curswant`) afterwards, and a Visual block
//! lays out its rows in screen columns, so these walk the whole line: linear since #1109, quadratic before
//! (one `$` did not finish in minutes).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use ruse_core::{apply_command, Command, EditorState, Motion, SelectKind};

/// Two lines of ~1 MB each of wide Hangul syllables (3 bytes, 2 cells), so `j`/`k` have a target.
fn source() -> Vec<u8> {
    let line = "가".repeat(350_000);
    format!("{line}\n{line}\n").into_bytes()
}

fn bench_long_line(c: &mut Criterion) {
    let bytes = source();
    let mut g = c.benchmark_group("long_line");
    g.sample_size(10);
    let mut case = |name: &str, setup: &[Command], timed: &[Command]| {
        g.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut st = EditorState::new(bytes.clone());
                    for cmd in setup {
                        apply_command(&mut st, cmd);
                    }
                    st
                },
                |mut st| {
                    for cmd in timed {
                        apply_command(&mut st, cmd);
                    }
                    st
                },
                BatchSize::LargeInput,
            )
        });
    };
    case("dollar", &[], &[Command::MoveLineEnd]);
    case(
        "j_k_at_end",
        &[Command::MoveLineEnd],
        &[Command::Move(1, Motion::Down), Command::Move(1, Motion::Up)],
    );
    case(
        "visual_block_yank",
        &[],
        &[
            Command::EnterVisual {
                kind: SelectKind::Blockwise,
            },
            Command::MoveLineEnd,
            Command::Move(1, Motion::Down),
            Command::YankSelection,
        ],
    );
    g.finish();
}

criterion_group!(benches, bench_long_line);
criterion_main!(benches);
