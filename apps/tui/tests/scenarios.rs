//! Curated cross-feature SCENARIO suite (testing-and-benchmarks.md §1.9): hand-written realistic
//! multi-step editing sessions with EXACT expected end-states, driven through the same stack `main.rs`
//! composes (input engine + Workspace). Where the oracle corpus checks single ops and the fuzzer checks
//! invariants, these pin the KNOWN-TRICKY cross-feature interactions a modal editor must get right —
//! dot-repeat × count, named registers, visual + operator, search → change → repeat, substitute + undo,
//! insert-session undo grouping, blockwise-insert replication, and multi-window buffer sharing — each as
//! a readable regression guard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{SplitDir, SubFlags, Workspace};
use ruse_tui::input::{parse_ex, Ex, Feed, GlobalPayload, InputEngine};

/// Route one engine outcome exactly as `main.rs::run` does: edits through the swap-trick `Workspace`,
/// `:s`/`:g` through the substitute/global engines, everything else per its `Feed`.
fn step(e: &mut InputEngine, ws: &mut Workspace, key: KeyEvent) {
    let mode = ws.focused().view.mode();
    match e.feed(key, mode) {
        Feed::Cmd(cmd) => {
            ws.apply(&cmd);
        }
        Feed::Replay(cmds) => {
            for c in &cmds {
                ws.apply(c);
            }
        }
        Feed::ExecuteEx(text) => match parse_ex(&text) {
            Ex::Substitute(s) => {
                let _ = ws.substitute(
                    s.range,
                    &s.pattern,
                    &s.replacement,
                    SubFlags {
                        global: s.global,
                        ignore_case: s.ignore_case,
                    },
                );
            }
            Ex::Global(g) => {
                // These scenarios drive only the core `d`/`s` payloads; a `normal` payload would need the
                // input engine (the frontend run loop's `run_global_normal`), out of scope for this helper.
                if let GlobalPayload::Core(cmd) = &g.cmd {
                    let _ = ws.global(g.range, &g.pattern, g.negate, cmd);
                }
            }
            _ => {}
        },
        Feed::Pending | Feed::Ignored => {}
    }
}

/// Feed a Vim-style keystroke script: literal chars, plus `<Esc>` / `<CR>` / `<BS>` / `<C-x>` tokens.
fn feed_str(e: &mut InputEngine, ws: &mut Workspace, s: &str) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let key = if c == '<' {
            let mut tag = String::new();
            for d in chars.by_ref() {
                if d == '>' {
                    break;
                }
                tag.push(d);
            }
            match tag.as_str() {
                "Esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                "CR" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                "BS" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                t if t.starts_with("C-") => KeyEvent::new(
                    KeyCode::Char(t.chars().nth(2).expect("C-<char>")),
                    KeyModifiers::CONTROL,
                ),
                _ => continue,
            }
        } else {
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
        };
        step(e, ws, key);
    }
}

fn buf(ws: &Workspace) -> String {
    String::from_utf8_lossy(ws.focused().doc.bytes()).into_owned()
}

/// A fresh engine + single-window workspace over `initial`.
fn session(initial: &str) -> (InputEngine, Workspace) {
    (
        InputEngine::new(),
        Workspace::new(initial.as_bytes().to_vec()),
    )
}

#[test]
fn insert_session_is_a_single_undo_group() {
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    assert_eq!(buf(&ws), "hello");
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "", "the whole insert session undoes as one group");
}

#[test]
fn dot_repeats_a_change_on_the_next_word() {
    let (mut e, mut ws) = session("foo foo");
    feed_str(&mut e, &mut ws, "cwbar<Esc>");
    assert_eq!(buf(&ws), "bar foo");
    feed_str(&mut e, &mut ws, "w.");
    assert_eq!(
        buf(&ws),
        "bar bar",
        "`.` replays the change on the next word"
    );
}

#[test]
fn count_multiplies_a_dot_repeat() {
    let (mut e, mut ws) = session("aaaaa");
    feed_str(&mut e, &mut ws, "x"); // delete one -> "aaaa"
    assert_eq!(buf(&ws), "aaaa");
    feed_str(&mut e, &mut ws, "3."); // repeat `x` with a fresh count of 3
    assert_eq!(buf(&ws), "a", "`3.` re-applies the last change three times");
}

#[test]
fn named_register_yank_then_paste() {
    let (mut e, mut ws) = session("one\ntwo");
    feed_str(&mut e, &mut ws, "\"ayy"); // yank line 1 into register a
    feed_str(&mut e, &mut ws, "j\"ap"); // paste it after line 2
    assert_eq!(
        buf(&ws),
        "one\ntwo\none",
        "named register round-trips through paste"
    );
}

#[test]
fn search_then_change_then_next_then_dot() {
    let (mut e, mut ws) = session("x foo y foo");
    feed_str(&mut e, &mut ws, "/foo<CR>"); // to the first foo
    feed_str(&mut e, &mut ws, "cwZ<Esc>"); // -> "x Z y foo"
    assert_eq!(buf(&ws), "x Z y foo");
    feed_str(&mut e, &mut ws, "n."); // next match, repeat the change
    assert_eq!(buf(&ws), "x Z y Z", "search-next + dot-repeat compose");
}

#[test]
fn substitute_then_undo_restores() {
    let (mut e, mut ws) = session("aba");
    feed_str(&mut e, &mut ws, ":s/a/X/g<CR>");
    assert_eq!(buf(&ws), "XbX");
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "aba", "a substitute is one undo group");
}

#[test]
fn visual_operator_deletes_the_selection() {
    let (mut e, mut ws) = session("hello world");
    feed_str(&mut e, &mut ws, "ved"); // v, e (to end of "hello"), d
    assert_eq!(
        buf(&ws),
        " world",
        "visual `e` selects the word, `d` deletes it"
    );
}

#[test]
fn blockwise_insert_replicates_and_undoes_as_one_group() {
    let (mut e, mut ws) = session("aa\nbb\ncc");
    feed_str(&mut e, &mut ws, "<C-v>jjIX<Esc>"); // block over col 0 of 3 rows, insert X
    assert_eq!(
        buf(&ws),
        "Xaa\nXbb\nXcc",
        "block insert replicates down every row"
    );
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(
        buf(&ws),
        "aa\nbb\ncc",
        "the whole block insert undoes as one group"
    );
}

#[test]
fn split_windows_share_one_buffer_and_undo() {
    let (mut e, mut ws) = session("hello");
    ws.split(SplitDir::Horizontal); // two windows onto the SAME buffer (as `C-w s` does in main.rs)
    assert_eq!(ws.window_count(), 2);
    feed_str(&mut e, &mut ws, "x"); // delete under cursor in the focused window
                                    // Both panes borrow the one shared Document, so the edit is visible in BOTH.
    assert_eq!(String::from_utf8_lossy(ws.pane(0).doc.bytes()), "ello");
    assert_eq!(
        String::from_utf8_lossy(ws.pane(1).doc.bytes()),
        "ello",
        "a split shares the buffer — the edit shows in both windows"
    );
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "hello", "undo restores the shared buffer");
}
