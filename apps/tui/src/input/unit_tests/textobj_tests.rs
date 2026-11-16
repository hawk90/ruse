use super::{feed, k};
use crate::input::*;

fn pair(open: char, close: char, around: bool) -> Motion {
    Motion::Pair {
        open,
        close,
        around,
    }
}

#[test]
fn text_objects_compose() {
    assert_eq!(
        feed("diw"),
        Feed::Cmd(Command::Delete(1, Motion::InnerWord))
    );
    assert_eq!(
        feed("ciw"),
        Feed::Cmd(Command::Change(1, Motion::InnerWord))
    );
    assert_eq!(feed("daw"), Feed::Cmd(Command::Delete(1, Motion::AWord)));
}

#[test]
fn word_and_bigword_objects() {
    assert_eq!(
        feed("diW"),
        Feed::Cmd(Command::Delete(1, Motion::InnerBigWord))
    );
    assert_eq!(feed("daW"), Feed::Cmd(Command::Delete(1, Motion::ABigWord)));
}

#[test]
fn paragraph_and_sentence_objects() {
    assert_eq!(
        feed("yip"),
        Feed::Cmd(Command::Yank(1, Motion::InnerParagraph))
    );
    assert_eq!(
        feed("dap"),
        Feed::Cmd(Command::Delete(1, Motion::AParagraph))
    );
    assert_eq!(
        feed("dis"),
        Feed::Cmd(Command::Delete(1, Motion::InnerSentence))
    );
    assert_eq!(
        feed("das"),
        Feed::Cmd(Command::Delete(1, Motion::ASentence))
    );
}

#[test]
fn delimiter_pair_objects_and_aliases() {
    // Inner vs around.
    assert_eq!(
        feed("di("),
        Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
    );
    assert_eq!(
        feed("da("),
        Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
    );
    // Closer and `b` alias to the same `()` object as the opener.
    assert_eq!(
        feed("di)"),
        Feed::Cmd(Command::Delete(1, pair('(', ')', false)))
    );
    assert_eq!(
        feed("dab"),
        Feed::Cmd(Command::Delete(1, pair('(', ')', true)))
    );
    // Braces: `{`/`}`/`B` collapse.
    assert_eq!(
        feed("ci{"),
        Feed::Cmd(Command::Change(1, pair('{', '}', false)))
    );
    assert_eq!(
        feed("daB"),
        Feed::Cmd(Command::Delete(1, pair('{', '}', true)))
    );
    // Brackets and angles.
    assert_eq!(
        feed("di["),
        Feed::Cmd(Command::Delete(1, pair('[', ']', false)))
    );
    assert_eq!(
        feed("da]"),
        Feed::Cmd(Command::Delete(1, pair('[', ']', true)))
    );
    assert_eq!(
        feed("di<"),
        Feed::Cmd(Command::Delete(1, pair('<', '>', false)))
    );
}

#[test]
fn quote_objects() {
    assert_eq!(
        feed("da\""),
        Feed::Cmd(Command::Delete(
            1,
            Motion::Quote {
                ch: '"',
                around: true
            }
        ))
    );
    assert_eq!(
        feed("ci'"),
        Feed::Cmd(Command::Change(
            1,
            Motion::Quote {
                ch: '\'',
                around: false
            }
        ))
    );
    assert_eq!(
        feed("yi`"),
        Feed::Cmd(Command::Yank(
            1,
            Motion::Quote {
                ch: '`',
                around: false
            }
        ))
    );
}

#[test]
fn text_object_extends_a_visual_selection() {
    // In Visual, `i`/`a` begin a text object; it completes as a bare `Move` the core turns into a span.
    let mut e = InputEngine::new();
    let vis = Mode::Visual {
        kind: SelectKind::Charwise,
    };
    assert_eq!(e.feed(k('i'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('w'), vis),
        Feed::Cmd(Command::Move(1, Motion::InnerWord))
    );
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('i'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('('), vis),
        Feed::Cmd(Command::Move(1, pair('(', ')', false)))
    );
}

#[test]
fn tag_objects_resolve_in_visual() {
    // `it`/`at` now resolve via a core byte scan (see `tag_text_objects` for the operator forms). In
    // Visual they extend the selection like any other text object — `vit` → `Move(1, Tag{inner})`.
    let vis = Mode::Visual {
        kind: SelectKind::Charwise,
    };
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('i'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('t'), vis),
        Feed::Cmd(Command::Move(1, Motion::Tag { around: false }))
    );
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('a'), vis), Feed::Pending);
    assert_eq!(
        e.feed(k('t'), vis),
        Feed::Cmd(Command::Move(1, Motion::Tag { around: true }))
    );
}

#[test]
fn bare_i_still_enters_insert() {
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(k('i'), Mode::Normal),
        Feed::Cmd(Command::EnterInsert)
    );
}
