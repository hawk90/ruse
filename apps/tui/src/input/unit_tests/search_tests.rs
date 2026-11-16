use super::k;
use crate::input::*;

#[test]
fn slash_opens_search_and_n_repeats() {
    let mut e = InputEngine::new();
    // No prior search yet: a search with no pattern, which the core fails (E35, #1173).
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: String::new(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
    assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
    // Submitting the pattern yields a bare-move Search AND records it for `n`/`N`.
    assert_eq!(
        e.submit_search("foo".into(), false),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
    // After a FORWARD search, `n` repeats forward and `N` reverses to backward. Both re-issue a
    // `Command::Search` (so they can REAPPLY the offset), not the bare `SearchNext`/`SearchPrev`.
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
    assert_eq!(
        e.feed(k('N'), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
}

#[test]
fn question_opens_backward_search_and_n_repeats_backward() {
    let mut e = InputEngine::new();
    // `?` opens the backward search line; submitting yields a backward Search + records the direction.
    assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
    assert_eq!(e.cmdline().map(|c| c.0), Some('?'));
    assert_eq!(
        e.submit_search("foo".into(), true),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
    // After a BACKWARD search, `n` continues BACKWARD and `N` reverses to forward.
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
    assert_eq!(
        e.feed(k('N'), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
}

#[test]
fn operator_folds_into_backward_search() {
    // `d?bar` — the armed operator survives the minibuffer and folds into a backward Search.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.submit_search("bar".into(), true),
        Feed::Cmd(Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "bar".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
}

#[test]
fn count_before_question_selects_the_nth_backward_match() {
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.submit_search("foo".into(), true),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 2,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
}

#[test]
fn empty_backward_search_reuses_last_pattern_backward() {
    // `/foo<CR>` then `?<CR>` reuses "foo" but backward; a following `n` then continues backward.
    let mut e = InputEngine::new();
    e.feed(k('/'), Mode::Normal);
    assert!(matches!(e.submit_search("foo".into(), false), Feed::Cmd(_)));
    e.feed(k('?'), Mode::Normal);
    assert_eq!(
        e.submit_search(String::new(), true),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
            offset: SearchOffset::None,
        }),
        "empty `?<CR>` reuse flips direction to backward for n"
    );
}

#[test]
fn gn_and_gn_backward_emit_the_search_object_with_the_last_pattern() {
    let mut e = InputEngine::new();
    // Before any search, `gn` carries an empty pattern, which the core fails (E35, #1184).
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::SearchObject {
            op: SearchOp::Move,
            count: 1,
            pattern: String::new(),
            backward: false,
        })
    );
    // After a search, bare `gn` = the Move (Visual-select) object; `gN` sets backward.
    e.set_last_search("foo".into(), true);
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::SearchObject {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
        })
    );
    e.feed(k('g'), Mode::Normal);
    assert_eq!(
        e.feed(k('N'), Mode::Normal),
        Feed::Cmd(Command::SearchObject {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: true,
        })
    );
}

#[test]
fn operator_and_count_fold_into_gn() {
    let mut e = InputEngine::new();
    e.set_last_search("foo".into(), true);
    // `2cgn` → change the object, count 2 (advance to the 2nd match), pattern baked in.
    assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('c'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::SearchObject {
            op: SearchOp::Change,
            count: 2,
            pattern: "foo".into(),
            backward: false,
        })
    );
    // `dgn` → delete the object.
    assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        Feed::Cmd(Command::SearchObject {
            op: SearchOp::Delete,
            count: 1,
            pattern: "foo".into(),
            backward: false,
        })
    );
}

#[test]
fn empty_search_pattern_with_no_previous_search_fails_in_the_core() {
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('?'), Mode::Normal), Feed::Pending);
    // No prior search → the empty pattern has nothing to reuse: the search runs with no pattern, which
    // the core fails (E35, #1173), and nothing is recorded for `n`.
    assert_eq!(
        e.submit_search(String::new(), true),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: String::new(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
    assert_eq!(e.last_search(), None);
}

#[test]
fn g_backtick_and_g_quote_jump_keeping_the_jumplist() {
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('`'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Normal),
        Feed::KeepJumps(Command::GotoNamedMark('a'))
    );
    assert_eq!(e.feed(k('g'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('\''), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('.'), Mode::Normal),
        Feed::KeepJumps(Command::GotoLastChangeLine)
    );
    // Under an operator it is the operator over the mark, as `` d`a ``.
    for key in ['d', 'g', '`'] {
        assert_eq!(e.feed(k(key), Mode::Normal), Feed::Pending);
    }
    assert_eq!(
        e.feed(k('a'), Mode::Normal),
        Feed::Cmd(Command::OpToMark {
            op: MarkOp::Delete,
            name: 'a',
            linewise: false,
        })
    );
}

#[test]
fn slash_clears_the_transient_axes_but_folds_the_operator_into_the_search() {
    // `/` is a MOTION: the transient count/op/awaiting axes are cleared (so nothing leaks into the
    // minibuffer), but an armed operator/count is CAPTURED for the search to consume — `d/pat`,
    // `2/pat`. This is the fix for the old behaviour that dropped the operator on `/`.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
    assert!(e.normal.op.is_none() && e.normal.awaiting == Awaiting::Nothing && e.normal.count == 0);
    assert_eq!(
        e.submit_search("bar".into(), false),
        Feed::Cmd(Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "bar".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
}

#[test]
fn count_before_slash_selects_the_nth_match() {
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.submit_search("foo".into(), false),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 2,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
}

// --- search offsets (#475): the `/pat/{offset}` suffix is split off and parsed at submit. ---

fn move_search(pattern: &str, backward: bool, offset: SearchOffset) -> Feed {
    Feed::Cmd(Command::Search {
        op: SearchOp::Move,
        count: 1,
        pattern: pattern.into(),
        backward,
        offset,
    })
}

#[test]
fn split_search_offset_grammar() {
    use crate::input::split_search_offset;
    // Forward delimiter `/`.
    assert_eq!(
        split_search_offset("foo/e", '/'),
        ("foo".into(), Some(SearchOffset::End(0)))
    );
    assert_eq!(
        split_search_offset("foo/s+2", '/'),
        ("foo".into(), Some(SearchOffset::Start(2)))
    );
    assert_eq!(
        split_search_offset("foo/+1", '/'),
        ("foo".into(), Some(SearchOffset::Line(1)))
    );
    // No delimiter at all -> `None` (offset absent, so an empty-pattern repeat reuses the last one).
    assert_eq!(split_search_offset("foo", '/'), ("foo".into(), None));
    // A delimiter with an empty part -> `Some(None)`: an EXPLICIT no-offset (resets the last offset).
    assert_eq!(
        split_search_offset("foo/", '/'),
        ("foo".into(), Some(SearchOffset::None))
    );
    // An escaped delimiter stays in the pattern; the split is the first unescaped one.
    assert_eq!(
        split_search_offset("ab\\/cd/e", '/'),
        ("ab\\/cd".into(), Some(SearchOffset::End(0)))
    );
    // A delimiter inside `[...]` is part of the pattern (#803, nvim `skip_regexp`).
    assert_eq!(split_search_offset("[/]", '/'), ("[/]".into(), None));
    assert_eq!(
        split_search_offset("[/]/e", '/'),
        ("[/]".into(), Some(SearchOffset::End(0)))
    );
    // Backward delimiter `?` (a `/` in the pattern is then literal).
    assert_eq!(
        split_search_offset("foo?e", '?'),
        ("foo".into(), Some(SearchOffset::End(0)))
    );
    // Empty pattern with an offset (`//e`): pattern empty, offset present.
    assert_eq!(
        split_search_offset("/e", '/'),
        (String::new(), Some(SearchOffset::End(0)))
    );
}

#[test]
fn submit_search_parses_the_offset_suffix() {
    let mut e = InputEngine::new();
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search("foo/e".into(), false),
        move_search("foo", false, SearchOffset::End(0))
    );
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search("bar/s-1".into(), false),
        move_search("bar", false, SearchOffset::Start(-1))
    );
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search("baz/+2".into(), false),
        move_search("baz", false, SearchOffset::Line(2))
    );
    // Backward: the `?` is the delimiter.
    e.feed(k('?'), Mode::Normal);
    assert_eq!(
        e.submit_search("qux?e".into(), true),
        move_search("qux", true, SearchOffset::End(0))
    );
}

#[test]
fn n_reapplies_the_offset() {
    // `/foo/e<CR>` records offset `e`; `n` re-issues a Search carrying the SAME offset.
    let mut e = InputEngine::new();
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search("foo/e".into(), false),
        move_search("foo", false, SearchOffset::End(0))
    );
    assert_eq!(
        e.feed(k('n'), Mode::Normal),
        move_search("foo", false, SearchOffset::End(0)),
        "n reapplies the stored offset"
    );
    assert_eq!(
        e.feed(k('N'), Mode::Normal),
        move_search("foo", true, SearchOffset::End(0)),
        "N reapplies the offset, flipped direction"
    );
}

#[test]
fn empty_pattern_reuses_or_replaces_the_offset() {
    // `/foo/e` then a BARE `/<CR>` (no delimiter typed) reuses BOTH the pattern and the offset (nvim).
    let mut e = InputEngine::new();
    e.feed(k('/'), Mode::Normal);
    e.submit_search("foo/e".into(), false);
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search(String::new(), false),
        move_search("foo", false, SearchOffset::End(0)),
        "bare `/<CR>` reuses pattern AND offset"
    );
    // `//<CR>` (a typed delimiter, empty offset) reuses the pattern but EXPLICITLY resets the offset
    // to none — nvim: `/foo/e` then `//` lands on the match start, not its end.
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search("/".into(), false),
        move_search("foo", false, SearchOffset::None),
        "`//` reuses pattern, resets offset"
    );
    // `//s` (empty pattern, NEW offset) reuses the pattern but REPLACES the offset.
    e.feed(k('/'), Mode::Normal);
    assert_eq!(
        e.submit_search("/s".into(), false),
        move_search("foo", false, SearchOffset::Start(0)),
        "`//s` reuses pattern, replaces offset"
    );
}
