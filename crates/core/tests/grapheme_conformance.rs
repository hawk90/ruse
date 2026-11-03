//! Unicode grapheme-cluster conformance through ruse's own motions and edits (#909, F-002).
//!
//! `tests/unicode/GraphemeBreakTest-17.0.0.txt` is the Unicode Character Database's conformance file
//! (<https://www.unicode.org/Public/17.0.0/ucd/auxiliary/GraphemeBreakTest.txt>, committed byte for byte;
//! Unicode 17.0.0 is the version of the pinned `unicode-segmentation` tables). Each line is a string with
//! its extended grapheme cluster boundaries marked `÷` (break) and `×` (no break). The segmentation crate
//! is tested against it upstream; this file checks that ruse's own layers above it never split one of
//! those clusters: the position conversions, `l`/`h` stepping, `$`, and `x` deleting one cluster at a
//! time. The cell-layout half lives in the renderer's tests (`apps/tui/src/ui/render.rs`).

use ruse_core::pos::{byte_to_grapheme, grapheme_to_byte, GraphemePos};
use ruse_core::{apply_command, BytePos, Command, EditorState, Motion};

/// One conformance case: the string and its cluster boundaries as byte offsets, `0` and the end included.
struct Case {
    line: usize,
    text: String,
    bounds: Vec<usize>,
}

fn cases() -> Vec<Case> {
    let data = include_str!("../../../tests/unicode/GraphemeBreakTest-17.0.0.txt");
    let mut out = Vec::new();
    for (i, raw) in data.lines().enumerate() {
        let body = raw.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let mut text = String::new();
        let mut bounds = Vec::new();
        for tok in body.split_whitespace() {
            match tok {
                "÷" => bounds.push(text.len()),
                "×" => {}
                hex => {
                    let cp = u32::from_str_radix(hex, 16).expect("a code point in hex");
                    text.push(char::from_u32(cp).expect("a scalar value"));
                }
            }
        }
        out.push(Case {
            line: i + 1,
            text,
            bounds,
        });
    }
    out
}

/// The data parses into the expected number of cases, and the pinned segmentation crate agrees with it:
/// a failure here means the committed file and the crate's Unicode version drifted apart.
#[test]
fn the_data_matches_the_pinned_segmenter() {
    use unicode_segmentation::UnicodeSegmentation;
    let all = cases();
    assert_eq!(all.len(), 766, "GraphemeBreakTest-17.0.0 has 766 cases");
    for c in &all {
        let mut got: Vec<usize> = c.text.grapheme_indices(true).map(|(i, _)| i).collect();
        got.push(c.text.len());
        assert_eq!(got, c.bounds, "line {}: {:?}", c.line, c.text);
    }
}

/// `byte_to_grapheme` / `grapheme_to_byte` (the F-002 column a user sees) count each cluster once.
#[test]
fn position_conversions_count_whole_clusters() {
    for c in cases() {
        for (g, &b) in c.bounds.iter().enumerate() {
            assert_eq!(
                byte_to_grapheme(&c.text, BytePos(b)).0,
                g,
                "line {}",
                c.line
            );
            assert_eq!(
                grapheme_to_byte(&c.text, GraphemePos(g)).0,
                b,
                "line {}",
                c.line
            );
        }
    }
}

/// The cases a single buffer line can hold: a `\n` ends the line, so those are the line motions' business.
fn one_line_cases() -> Vec<Case> {
    cases()
        .into_iter()
        .filter(|c| !c.text.contains('\n'))
        .collect()
}

/// `l` from the start visits every cluster start once, in order, and stops on the last cluster (Normal
/// mode rests on a character); `$` lands on the last one; `h` from there walks the same starts back.
#[test]
fn l_and_h_step_one_cluster_at_a_time() {
    let mut n = 0;
    for c in one_line_cases() {
        let starts = &c.bounds[..c.bounds.len() - 1];
        let last = *starts.last().expect("a case has a cluster");
        let mut st = EditorState::new(c.text.clone().into_bytes());
        let mut seen = vec![st.cursor().0];
        for _ in 1..starts.len() {
            apply_command(&mut st, &Command::Move(1, Motion::Right));
            seen.push(st.cursor().0);
        }
        assert_eq!(seen, starts, "`l` over line {}: {:?}", c.line, c.text);
        apply_command(&mut st, &Command::Move(1, Motion::Right));
        assert_eq!(
            st.cursor().0,
            last,
            "`l` stops on the last cluster, line {}",
            c.line
        );

        let mut st = EditorState::new(c.text.clone().into_bytes());
        apply_command(&mut st, &Command::Move(1, Motion::LineEnd));
        assert_eq!(st.cursor().0, last, "`$` on line {}: {:?}", c.line, c.text);
        let mut back = vec![st.cursor().0];
        for _ in 1..starts.len() {
            apply_command(&mut st, &Command::Move(1, Motion::Left));
            back.push(st.cursor().0);
        }
        back.reverse();
        assert_eq!(back, starts, "`h` over line {}: {:?}", c.line, c.text);
        st.assert_invariants();
        n += 1;
    }
    assert!(n > 600, "most cases fit on one line ({n})");
}

/// `x` from the start deletes exactly one cluster each time, leaving the text after it.
#[test]
fn x_deletes_one_cluster() {
    for c in one_line_cases() {
        let mut st = EditorState::new(c.text.clone().into_bytes());
        for &b in &c.bounds[1..] {
            apply_command(&mut st, &Command::DeleteUnder(1));
            assert_eq!(
                st.bytes(),
                &c.text.as_bytes()[b..],
                "`x` on line {}: {:?}",
                c.line,
                c.text
            );
            assert_eq!(st.cursor().0, 0, "line {}", c.line);
        }
        st.assert_invariants();
    }
}

/// Every other horizontal motion (`w` `b` `e` `ge` and the WORD forms, `0`, `^`, `f`/`t` to each char)
/// from every cluster start lands on a cluster start: none of them stops inside a cluster.
#[test]
fn horizontal_motions_land_on_cluster_starts() {
    let motions = [
        Motion::WordFwd,
        Motion::WordBack,
        Motion::WordEnd,
        Motion::BigWordFwd,
        Motion::BigWordBack,
        Motion::BigWordEnd,
        Motion::LineStart,
    ];
    for c in one_line_cases() {
        let starts = &c.bounds[..c.bounds.len() - 1];
        for (i, _) in starts.iter().enumerate() {
            let mut finds: Vec<Motion> = Vec::new();
            for ch in c.text.chars() {
                for (forward, till) in [(true, false), (false, false), (true, true), (false, true)]
                {
                    finds.push(Motion::FindChar {
                        ch,
                        forward,
                        till,
                        repeat: false,
                    });
                }
            }
            for m in motions.iter().copied().chain(finds) {
                let mut st = EditorState::new(c.text.clone().into_bytes());
                for _ in 0..i {
                    apply_command(&mut st, &Command::Move(1, Motion::Right));
                }
                apply_command(&mut st, &Command::Move(1, m));
                assert!(
                    starts.contains(&st.cursor().0),
                    "{m:?} from cluster {i} of line {} ({:?}) rests at byte {}",
                    c.line,
                    c.text,
                    st.cursor().0
                );
            }
        }
    }
}
