//! Ex edits replay from a saved trace (#1105, RFC-0020, D-061).
//!
//! Each session runs through the headless harness (the real key path: the input engine, the Ex dispatch,
//! the `:s///c` loop, `:normal`), saves its trace with `:trace save`, and replays it with the library
//! replay `ruse --replay` uses. The replayed document and cursor must equal the live ones. Before RFC-0020
//! the Ex edits were missing from the trace, so these replays printed the document without them.

use proptest::prelude::*;
use ruse_core::Trace;
use ruse_tui::headless::Headless;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique scratch path per call (parallel-test safe).
fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "ruse-trace-ex-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Run `keys` on `text`, save the trace with `:trace save`, and return the session and the saved trace.
fn session(text: &str, keys: &str) -> (Headless, String) {
    let mut h = Headless::vim(text);
    h.keys(keys);
    let path = scratch("t.trace");
    h.keys(&format!("<Esc>:trace save {}<CR>", path.display()));
    assert!(
        h.status().starts_with("trace saved"),
        "{keys:?}: {}",
        h.status()
    );
    let saved = std::fs::read_to_string(&path).expect("read the saved trace");
    let _ = std::fs::remove_file(&path);
    (h, saved)
}

/// The replay of `trace` onto `text` ends where the live session did: same document, cursor and mode.
fn assert_replays(text: &str, keys: &str, h: &Headless, trace: &str) {
    let st = Trace::from_text(trace)
        .unwrap_or_else(|e| panic!("{keys:?}: the saved trace parses: {e:?}\n{trace}"))
        .replay(text.as_bytes())
        .expect("replay");
    assert_eq!(
        String::from_utf8_lossy(st.bytes()),
        h.text(),
        "{keys:?}: replayed document\n{trace}"
    );
    assert_eq!(
        st.cursor(),
        h.cursor(),
        "{keys:?}: replayed cursor\n{trace}"
    );
    assert_eq!(st.mode(), h.mode(), "{keys:?}: replayed mode");
}

/// The representative sessions: each Ex edit the RFC names, mixed with typed keys.
const SESSIONS: &[(&str, &str, &str)] = &[
    // (initial text, keys, an Ex verb the trace must hold)
    (
        "foo bar\nfoo baz\nqux\n",
        "wcwzap<Esc>:%s/foo/X/g<CR>",
        "ex_substitute",
    ),
    ("x b\nb\nb x\n", "/b<CR>:s//Y/<CR>", "ex_substitute"),
    ("a\nx1\nb\nx2\nc\n", "j:g/x/d<CR>", "ex_global d"),
    ("a\nx1\nb\nx2\nc\n", ":g/x/s/x/Z/<CR>", "ex_global s"),
    ("a1\nb\na2\n", ":g/a/normal Ax<CR>", "ex_begin global"),
    ("a\nb\nc\n", ":%normal I-<CR>", "ex_begin normal"),
    ("a\nb\n", ":normal! Ax<CR>", "ex_begin normal"),
    ("c\nb\na\nb\n", ":sort u<CR>", "ex_sort"),
    ("1\n2\n3\n4\n", ":2m0<CR>:3t$<CR>", "ex_move"),
    ("1\n2\n3\n4\n", ":1t.<CR>jdd", "ex_copy"),
    ("top\nend\n", ":r !echo x<CR>", "ex_read"),
    ("c\na\nb\n", ":%!sort<CR>", "ex_filter"),
    ("c\na\nb\n", "j:.,$!sort<CR>", "ex_filter"),
    ("aa\naa\naa\n", ":s/a/b/<CR>j&", "ex_substitute"),
    ("aa\naa\naa\n", ":s/a/b/<CR>jg&", "ex_substitute"),
    ("aa\naa\naa\n", ":s/a/b/g<CR>j:&&<CR>", "ex_substitute"),
    // The 4th match is still asking when the session's closing `<Esc>` quits the loop.
    ("a a a\na\n", ":%s/a/b/gc<CR>yny", "c:ynyn"),
    ("a a a\na\n", ":%s/a/b/gc<CR>nq", "c:nnnn"),
    ("a a a\na\n", ":%s/a/b/gc<CR>na", "c:nyyy"),
    ("a a a\na\n", ":%s/a/b/gc<CR>yl", "c:yynn"),
    ("one\ntwo\nthree\n", ":2d a<CR>\"ap", "ex_delete"),
    ("one\ntwo\nthree\n", ":2,3y b<CR>G\"bp", "ex_yank"),
    ("one\n  two\nthree\n", ":.,+1j<CR>", "ex_join"),
    ("one\ntwo\n", ":set sw=2<CR>>>:2>><CR>", "ex_shift"),
    ("one\ntwo\n", "yy:put<CR>:0put<CR>", "ex_put"),
    ("one\ntwo\nthree\n", ":3mark a<CR>gg'ax", "ex_mark"),
    ("\tone\n", ":set et<CR>:retab<CR>", "ex_retab"),
    // ruse's shipped `ignorecase`+`smartcase` are in the trace's starting options: `/foo` lands on `Foo`.
    ("x foo Foo\n", "/foo<CR>x", "ex_set smartcase 1"),
    // One `u` undoes a whole `:g/normal`, `:normal` or macro after replay, as it did live.
    ("a1\nb\na2\n", ":g/a/normal Ax<CR>u", "ex_end"),
    ("a\nb\n", "Ay<Esc>:%normal Az<CR>u", "ex_end"),
    ("a\nb\nc\n", "qaAq<Esc>jq@au", "ex_begin macro"),
    ("a\nb\nc\n", "xjxu:1d<CR>u", "ex_undo_sync"),
];

#[test]
fn saved_traces_of_ex_sessions_replay_to_the_live_state() {
    for &(text, keys, verb) in SESSIONS {
        let (h, saved) = session(text, keys);
        assert!(
            saved.contains(verb),
            "{keys:?}: the trace holds `{verb}`\n{saved}"
        );
        assert_replays(text, keys, &h, &saved);
    }
}

/// The saved file is what [`Headless::trace`] serializes, so the property test below covers it too.
#[test]
fn trace_save_writes_the_session_trace() {
    let (h, saved) = session("a\nb\n", ":%s/a/c/<CR>");
    assert_eq!(Trace::from_text(&saved), Ok(h.trace()));
}

/// `ruse --replay` on a saved trace prints the live document (the process boundary, F-022).
#[test]
fn ruse_replay_reproduces_an_ex_session() {
    let text = "c\nx a\nb\nx d\n";
    let keys = ":%!sort<CR>:g/x/s/x/y/<CR>:2,3m$<CR>:r !echo z<CR>";
    let (h, saved) = session(text, keys);
    let (tp, fp) = (scratch("bin.trace"), scratch("bin.txt"));
    std::fs::write(&tp, &saved).unwrap();
    std::fs::write(&fp, text).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ruse"))
        .arg("--replay")
        .arg(&tp)
        .arg(&fp)
        .output()
        .expect("spawn ruse --replay");
    let _ = (std::fs::remove_file(&tp), std::fs::remove_file(&fp));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), h.text());
}

/// Replay never runs the command a `:r !cmd` or `:{range}!cmd` ran: its output is in the trace.
#[test]
fn replay_uses_the_recorded_output_not_the_command() {
    let (h, saved) = session("a\n", ":r !echo $$<CR>");
    // A second run of `echo $$` would print another pid; replay must give the live one.
    assert_replays("a\n", ":r !echo $$", &h, &saved);
}

/// Keys and Ex commands a random session is made of. Every Ex edit the trace records is here, with undo,
/// redo, dot-repeat and a macro so the undo grouping is exercised.
const PIECES: &[&str] = &[
    "x",
    "dd",
    "p",
    "u",
    // `<Esc>` first: in Insert, `CTRL-R` would read a special register (`":`, `"/`), which the trace does
    // not hold (RFC-0020 open question 1).
    "<Esc><C-r>",
    ".",
    "j",
    "k",
    "w",
    "G",
    "gg",
    "ihi<Esc>",
    "oab<Esc>",
    "A c<Esc>",
    ">>",
    ":s/a/b/<CR>",
    ":%s/b/c/g<CR>",
    ":s//X/<CR>",
    "/a<CR>",
    ":g/a/d<CR>",
    ":g/c/s/c/a/<CR>",
    ":v/a/normal Az<CR>",
    ":%normal Ay<CR>",
    ":normal x<CR>",
    ":sort<CR>",
    ":sort! u<CR>",
    ":2m0<CR>",
    ":1t$<CR>",
    ":%j<CR>",
    ":>><CR>",
    ":1d<CR>",
    ":y<CR>",
    ":put<CR>",
    "&",
    "g&",
    ":&&<CR>",
    ":%!sort<CR>",
    ":r !echo b<CR>",
    ":%s/a/A/gc<CR>yna",
    ":s/b/B/c<CR>q",
    ":set sw=1<CR>",
    ":2mark a<CR>'a",
    "qqxjq@q",
];

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Any session of keys and Ex edits, replayed from its trace, ends in the live document and cursor.
    #[test]
    fn random_sessions_with_ex_edits_replay_to_the_same_state(
        text in "[abc \n]{0,24}",
        picks in proptest::collection::vec(any::<proptest::sample::Index>(), 1..12),
    ) {
        let keys: String = picks.iter().map(|i| PIECES[i.index(PIECES.len())]).collect();
        let mut h = Headless::vim(&text);
        h.keys(&keys);
        h.keys("<Esc>");
        let trace = h.trace();
        let again = Trace::from_text(&trace.to_text());
        prop_assert_eq!(again.as_ref(), Ok(&trace), "the trace round-trips");
        let st = trace.replay(text.as_bytes()).expect("replay");
        prop_assert_eq!(
            String::from_utf8_lossy(st.bytes()),
            h.text(),
            "keys {:?}\n{}",
            keys,
            trace.to_text()
        );
        prop_assert_eq!(st.cursor(), h.cursor(), "keys {:?}\n{}", keys, trace.to_text());
    }
}
