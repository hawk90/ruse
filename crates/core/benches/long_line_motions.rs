//! #1224: word and grapheme motions on one very long line (a minified file, a large paste). Each case runs
//! a motion as N separate keypresses (`w`, `b`, `e`, `W`, `B`, `ge`, `i_CTRL-W`) over a single-line buffer of
//! 64 KiB, 1 MiB and 16 MiB, plus the same on a normal file of 100-column lines so the common case is
//! guarded. Before #1224 every grapheme step re-validated and re-segmented the whole line, so one word
//! motion cost O(line) per character crossed (a 64 MiB line ran > 120 s).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use ruse_core::{apply_command, Command, EditorState, Motion};

/// Minified-code-like text: identifiers, punctuation, spaces, a Hangul word and a combining mark.
const UNIT: &str = "{\"ab_cd\":12, ef.gh(\"가나\"); e\u{301}x=[1,2]} ";

fn one_line(bytes: usize) -> Vec<u8> {
    let mut s = UNIT.repeat(bytes / UNIT.len() + 1);
    s.truncate(s.floor_char_boundary(bytes));
    s.push('\n');
    s.into_bytes()
}

/// The start of the char holding byte `i` of the UTF-8 text `b`.
fn char_start(b: &[u8], mut i: usize) -> usize {
    while i > 0 && b[i] & 0xC0 == 0x80 {
        i -= 1;
    }
    i
}

/// ~1 MiB of 100-column lines: the common case.
fn normal_file() -> Vec<u8> {
    let mut line = UNIT.repeat(100 / UNIT.len() + 1);
    line.truncate(line.floor_char_boundary(100));
    format!("{line}\n").repeat(10_000).into_bytes()
}

fn bench_long_line_motions(c: &mut Criterion) {
    let mut g = c.benchmark_group("long_line_motions");
    g.sample_size(10);
    let inputs = [
        ("64k", one_line(64 << 10), 100),
        ("1m", one_line(1 << 20), 100),
        ("16m", one_line(16 << 20), 10),
        ("100col", normal_file(), 100),
    ];
    for (size, bytes, n) in &inputs {
        let mut case = |name: &str, setup: &[Command], key: Command| {
            g.bench_function(format!("{name}/{size}"), |b| {
                b.iter_batched(
                    || {
                        let mut st = EditorState::new(bytes.clone());
                        for cmd in setup {
                            apply_command(&mut st, cmd);
                        }
                        st
                    },
                    |mut st| {
                        for _ in 0..*n {
                            apply_command(&mut st, &key);
                        }
                        st
                    },
                    BatchSize::LargeInput,
                )
            });
        };
        // The backward cases start at the far end: the end of the line, or the last line of the file.
        let end = [Command::Move(1, Motion::LastLine), Command::MoveLineEnd];
        case("w", &[], Command::Move(1, Motion::WordFwd));
        case("W", &[], Command::Move(1, Motion::BigWordFwd));
        case("e", &[], Command::Move(1, Motion::WordEnd));
        case("b", &end, Command::Move(1, Motion::WordBack));
        case("B", &end, Command::Move(1, Motion::BigWordBack));
        case("ge", &end, Command::Move(1, Motion::WordEndBack));
        case(
            "i_ctrl_w",
            &[Command::Move(1, Motion::LastLine), Command::AppendLineEnd],
            Command::InsertDeleteWordBack,
        );
    }
    // #1237: the same `i_CTRL-W` with the 26 marks `a`-`z` spread over the line. Every edit carries each
    // tracked position (marks, change list, `[`/`]`) through the change by line; before #1237 each one
    // scanned its line, so this cost O(line × positions).
    for (size, bytes, n) in &inputs {
        g.bench_function(format!("i_ctrl_w_marks/{size}"), |b| {
            b.iter_batched(
                || {
                    let mut st = EditorState::new(bytes.clone());
                    // The content end: a long line's end, or the last line of the normal file.
                    let end = bytes.len() - 1;
                    for (k, c) in ('a'..='z').enumerate() {
                        st.set_cursor(ruse_core::BytePos(char_start(bytes, end * k / 26)));
                        apply_command(&mut st, &Command::SetNamedMark(c));
                    }
                    apply_command(&mut st, &Command::Move(1, Motion::LastLine));
                    apply_command(&mut st, &Command::AppendLineEnd);
                    st
                },
                |mut st| {
                    for _ in 0..*n {
                        apply_command(&mut st, &Command::InsertDeleteWordBack);
                    }
                    st
                },
                BatchSize::LargeInput,
            )
        });
    }
    // One word as long as the line (a run of digits, a base64 blob): `e`, `b` and `i_CTRL-W` cross all of it
    // in one keypress. This is the #1213 fuzz case (`i_CTRL-W` after a 64 MiB put of one char).
    for (size, len) in [("64k", 64usize << 10), ("1m", 1 << 20), ("16m", 16 << 20)] {
        let bytes = format!("{}\n", "ab_12".repeat(len / 5)).into_bytes();
        let mut case = |name: &str, setup: &[Command], key: Command| {
            g.bench_function(format!("{name}/{size}"), |b| {
                b.iter_batched(
                    || {
                        let mut st = EditorState::new(bytes.clone());
                        for cmd in setup {
                            apply_command(&mut st, cmd);
                        }
                        st
                    },
                    |mut st| {
                        apply_command(&mut st, &key);
                        st
                    },
                    BatchSize::LargeInput,
                )
            });
        };
        case("one_word_e", &[], Command::Move(1, Motion::WordEnd));
        case(
            "one_word_b",
            &[Command::MoveLineEnd],
            Command::Move(1, Motion::WordBack),
        );
        case(
            "one_word_i_ctrl_w",
            &[Command::AppendLineEnd],
            Command::InsertDeleteWordBack,
        );
    }
    // The other motions the same input reaches, 3 keypresses each: `;` after `f;`, `)` / `(` (the unit has
    // no sentence end, so each walks the whole line), `%`, `{count}|`, `0`, `^`, `$`.
    for (size, bytes, _) in &inputs {
        let mut case = |name: &str, setup: &[Command], key: Command| {
            g.bench_function(format!("{name}/{size}"), |b| {
                b.iter_batched(
                    || {
                        let mut st = EditorState::new(bytes.clone());
                        for cmd in setup {
                            apply_command(&mut st, cmd);
                        }
                        st
                    },
                    |mut st| {
                        for _ in 0..3 {
                            apply_command(&mut st, &key);
                        }
                        st
                    },
                    BatchSize::LargeInput,
                )
            });
        };
        let find = |repeat| Motion::FindChar {
            ch: ';',
            forward: true,
            till: false,
            repeat,
        };
        let end = [Command::Move(1, Motion::LastLine), Command::MoveLineEnd];
        case(
            "f_semicolon",
            &[Command::Move(1, find(false))],
            Command::Move(1, find(true)),
        );
        case("sentence_fwd", &[], Command::Move(1, Motion::SentenceFwd));
        case(
            "sentence_back",
            &end,
            Command::Move(1, Motion::SentenceBack),
        );
        case("percent", &[], Command::Move(1, Motion::MatchBracket));
        case("pipe", &end, Command::Move(5000, Motion::Column));
        case("zero", &end, Command::Move(1, Motion::LineStart));
        case("caret", &end, Command::Move(1, Motion::LineFirstNonBlank));
        case("dollar", &[], Command::MoveLineEnd);
    }
    g.finish();
}

criterion_group!(benches, bench_long_line_motions);
criterion_main!(benches);
