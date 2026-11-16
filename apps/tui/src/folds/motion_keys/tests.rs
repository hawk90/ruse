use super::*;

#[test]
fn parse_fold_motion_resolves_the_common_motions() {
    use FoldMotionParse::Ready;
    // Single-key linewise motions, no count.
    assert_eq!(
        parse_fold_motion("j"),
        Ready {
            motion: Motion::Down,
            count: 1
        }
    );
    assert_eq!(
        parse_fold_motion("k"),
        Ready {
            motion: Motion::Up,
            count: 1
        }
    );
    // Bare `G` → last line; `{count}G` → GotoLine with the count.
    assert_eq!(
        parse_fold_motion("G"),
        Ready {
            motion: Motion::LastLine,
            count: 1
        }
    );
    assert_eq!(
        parse_fold_motion("5G"),
        Ready {
            motion: Motion::GotoLine,
            count: 5
        }
    );
    // A count then a line motion (`zf3j`).
    assert_eq!(
        parse_fold_motion("3j"),
        Ready {
            motion: Motion::Down,
            count: 3
        }
    );
    // Paragraph motions.
    assert_eq!(
        parse_fold_motion("}"),
        Ready {
            motion: Motion::ParagraphFwd,
            count: 1
        }
    );
    assert_eq!(
        parse_fold_motion("{"),
        Ready {
            motion: Motion::ParagraphBack,
            count: 1
        }
    );
}

#[test]
fn parse_fold_motion_awaits_multi_key_and_counts() {
    use FoldMotionParse::{More, Ready};
    // Multi-key leads await the second key.
    assert_eq!(parse_fold_motion("g"), More);
    assert_eq!(
        parse_fold_motion("gg"),
        Ready {
            motion: Motion::GotoLine,
            count: 1
        }
    );
    assert_eq!(parse_fold_motion("i"), More);
    assert_eq!(
        parse_fold_motion("ip"),
        Ready {
            motion: Motion::InnerParagraph,
            count: 1
        }
    );
    assert_eq!(parse_fold_motion("a"), More);
    assert_eq!(
        parse_fold_motion("ap"),
        Ready {
            motion: Motion::AParagraph,
            count: 1
        }
    );
    // A partial count alone awaits its motion.
    assert_eq!(parse_fold_motion("3"), More);
    assert_eq!(parse_fold_motion("12"), More);
}

#[test]
fn parse_fold_motion_cancels_unsupported() {
    use FoldMotionParse::Cancel;
    // A leading `0` is the column-0 motion, not a count: it folds the cursor line (`zf0`).
    assert_eq!(
        parse_fold_motion("0"),
        FoldMotionParse::Ready {
            motion: Motion::Line,
            count: 1
        }
    );
    // Unknown / non-fold motions abandon the pending zf.
    assert_eq!(parse_fold_motion("x"), Cancel);
    assert_eq!(parse_fold_motion("w"), Cancel);
    assert_eq!(parse_fold_motion("gx"), Cancel);
    assert_eq!(parse_fold_motion("ix"), Cancel);
}
