//! #1109: the wanted screen column (`curswant`, measured after every command since #1103) and the block
//! geometry walk a long non-ASCII line in linear time. Before the fix each character's width re-segmented
//! the whole line, so one `$` on a long CJK line took seconds. The bound is loose (debug build, loaded CI);
//! the quadratic version is orders of magnitude over it.

use std::time::{Duration, Instant};

use ruse_core::{apply_command, Command, EditorState, Motion, SelectKind};

/// Two long lines of `unit`, so `j`/`k` have somewhere to go.
fn two_lines(unit: &str, n: usize) -> Vec<u8> {
    let line = unit.repeat(n);
    format!("{line}\n{line}\n").into_bytes()
}

fn run(bytes: Vec<u8>, cmds: &[Command]) -> (EditorState, Duration) {
    let mut st = EditorState::new(bytes);
    let start = Instant::now();
    for c in cmds {
        apply_command(&mut st, c);
    }
    (st, start.elapsed())
}

const LIMIT: Duration = Duration::from_secs(5);

#[test]
fn vertical_moves_and_dollar_on_a_long_non_ascii_line_are_linear() {
    // 60k wide Hangul syllables (180 kB per line), and a combining-mark line (`e` + U+0301).
    for (unit, n) in [("가", 60_000), ("e\u{301}", 60_000)] {
        let bytes = two_lines(unit, n);
        let line_len = unit.len() * n;
        let (st, took) = run(
            bytes,
            &[
                Command::MoveLineEnd,
                Command::Move(1, Motion::Down),
                Command::Move(1, Motion::Up),
                Command::MoveLineStart,
                Command::Move(1, Motion::Down),
            ],
        );
        assert!(took < LIMIT, "{unit:?}: {took:?}");
        assert_eq!(st.cursor(), line_len + 1, "{unit:?}: column 0 of line 2");
    }
}

#[test]
fn a_visual_block_over_long_non_ascii_lines_is_linear() {
    let n = 60_000;
    let (st, took) = run(
        two_lines("가", n),
        &[
            Command::EnterVisual {
                kind: SelectKind::Blockwise,
            },
            Command::MoveLineEnd,
            Command::Move(1, Motion::Down),
            Command::YankSelection,
        ],
    );
    assert!(took < LIMIT, "{took:?}");
    assert_eq!(
        st.cursor(),
        0,
        "a block yank returns to the top-left corner"
    );
}
