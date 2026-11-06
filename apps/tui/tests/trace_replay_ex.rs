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
        st.cursor().0,
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
    // A `:g` running any other command records each command it ran, in one group (#1245).
    ("a1\na2\na3\na4\na5\n", ":g/a/d 2<CR>", "ex_begin global"),
    ("a1\nb1\na2\nb2\na3\n", ":g/^/m0<CR>", "ex_begin global"),
    ("a1\nb1\na2\nb2\na3\n", ":g/^/m0<CR>u", "ex_begin global"),
    ("a1\nb\na2\nb\na3\n", ":g/b/j<CR>", "ex_join"),
    ("a1\nb\na2\nb\na3\n", ":g/b/y A<CR>G\"ap", "ex_yank"),
    ("a1\nb1\na2\nb2\na3\n", ":g/a/s/1/X/|>|d<CR>p", "ex_shift"),
    ("a1\nb1\na2\nb2\na3\n", ":g/a/g/1/d<CR>", "ex_global d"),
    ("a1\nb1\na2\nb2\na3\n", ":g/./m-2<CR>", "ex_begin global"),
    ("a1\nb1\na2\nb2\na3\n", ":g/b/ka<CR>gg'ax", "ex_mark"),
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
    // `:mark` puts the mark on the first non-blank, and sets the non-letter marks too (#1241).
    ("one\n  two\nthree\n", ":2ka<CR>gg`ax", "ex_mark 2 97 ^"),
    ("one\ntwo\nthree\n", ":3k1<CR>gg`1x", "ex_mark 3 49 ^"),
    ("one\ntwo\nthree\n", ":2k<<CR>:3k><CR>gvd", "ex_mark 3 62 ^"),
    ("one\ntwo\nthree\n", ":3k'<CR>``x", "ex_mark 3 39 ^"),
    (
        "one\ntwo\nthree\n",
        ":2k[<CR>:3k]<CR>:'[,']d<CR>",
        "ex_mark 2 91 ^",
    ),
    ("\tone\n", ":set et<CR>:retab<CR>", "ex_retab"),
    // ruse's shipped `ignorecase`+`smartcase` are in the trace's starting options: `/foo` lands on `Foo`.
    ("x foo Foo\n", "/foo<CR>x", "ex_set smartcase 1"),
    // One `u` undoes a whole `:g/normal`, `:normal` or macro after replay, as it did live.
    ("a1\nb\na2\n", ":g/a/normal Ax<CR>u", "ex_end"),
    ("a\nb\n", "Ay<Esc>:%normal Az<CR>u", "ex_end"),
    ("a\nb\nc\n", "qaAq<Esc>jq@au", "ex_begin macro"),
    ("a\nb\nc\n", "xjxu:1d<CR>u", "ex_undo_sync"),
    // Cursor moves the session makes outside the core are recorded as placements (#1134): the line picker,
    // `z<CR>`/`z.`/`z-` with a count, the `zf{motion}` landing, `zj`/`zk` and `[z`/`]z`.
    ("abc\nxyz\nfoo\n", "<C-l>foo<CR>x", "ex_place"),
    ("  a\n  b\n  c\n", "3z<CR>x", "ex_place"),
    ("  a\n  b\n  c\n", "2z.x", "ex_place"),
    ("a\nb\nc\nd\n", "jzfkx", "ex_place"),
    ("a\nb\nc\nd\ne\n", "jjzfjzoggzjx", "ex_place"),
    ("a\nb\nc\nd\ne\n", "jzfjzoGzkx", "ex_place"),
    ("a\nb\nc\nd\ne\n", "jzfjjzo]zx[zx", "ex_place"),
    // Register values the session writes are recorded as `ex_register` (RFC-0020 R6): the read-only
    // `"/ ": ".` (a put, and Insert's `CTRL-R`), and a macro's keys (`"ap` puts the recorded `x`).
    ("abc\n", "/b<CR>\"/P", "ex_register 47 c 62"),
    ("a\n", ":s/a/b/<CR>\":p", "ex_register 58 c"),
    ("x\n", "ihi<Esc>\".p", "ex_register 46 c 6869"),
    ("x\n", ":s/x/y/<CR>A <C-r>:<C-r>/<Esc>", "ex_register 58 c"),
    ("ab\n", "qaxq\"ap", "ex_register 97 c 78"),
    ("ab\n", "qaxqqAlq\"ap", "ex_register 97 c 786c"),
    // `:keepjumps` skips the jump `:d` makes; replay skips it too, so `''` goes where it went live.
    ("a\nb\nc\nd\n", "G:keepjumps 1d<CR>''x", "ex_keepjumps 1"),
    (
        "a\nb\nc\nd\n",
        "G:keepjumps 2,3s/./X/<CR>``x",
        "ex_keepjumps 0",
    ),
];

/// Emacs-profile sessions replay with point between characters (#1134): the trace starts with the caret
/// gravity, so `C-e` then an edit at the line end lands where it did live.
const EMACS_SESSIONS: &[(&str, &str)] = &[
    ("abc\ndef\n", "<C-e>x"),
    ("abc\ndef\n", "<C-e><C-d>"),
    ("abc\ndef\n", "<C-n><C-e>yz<C-a><C-d>"),
    ("abc\n", "<M->>q"),
    ("abc def\n", "<M-f><M-f><C-b><C-k>"),
];

#[test]
fn saved_traces_of_emacs_sessions_replay_to_the_live_state() {
    for &(text, keys) in EMACS_SESSIONS {
        let mut h = Headless::emacs(text);
        h.keys(keys);
        let trace = h.trace().to_text();
        assert!(
            trace.contains("ex_set caretgravity 1"),
            "{keys:?}: the trace holds the caret gravity\n{trace}"
        );
        assert_replays(text, keys, &h, &trace);
    }
}

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
    for keys in [
        ":%!sort<CR>:g/x/s/x/y/<CR>:2,3m$<CR>:r !echo z<CR>",
        // A per-line `:g` (#1245).
        ":g/^/m0<CR>:g/x/d 2<CR>:g/./j|s/^/-/<CR>",
    ] {
        ruse_replay_matches(text, keys);
    }
}

fn ruse_replay_matches(text: &str, keys: &str) {
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

/// A language server's edits (formatting, rename, code action, completion) are recorded as the resolved
/// edit list (`ex_edits`, RFC-0020 R6). No server runs headless, so the edit list is synthetic; it goes
/// through the same session path the LSP coordinator uses. Replay gives the live document and cursor, and
/// the batch undoes as one step, as it did live.
#[test]
fn lsp_edits_replay_from_the_trace() {
    let text = "fn  main ( ) {}\nlet x=1;\n";
    let mut h = Headless::vim(text);
    h.keys("jw");
    assert!(h.lsp_edits(&[(2, 4, " "), (8, 13, "()"), (21, 22, " = ")]));
    h.keys("x");
    // A refused batch (overlapping edits) is recorded too, and refused again on replay.
    assert!(!h.lsp_edits(&[(0, 3, "a"), (2, 4, "b")]));
    let trace = h.trace().to_text();
    assert!(
        trace.contains("ex_edits lsp 2:4:20 8:13:2829 21:22:203d20"),
        "{trace}"
    );
    assert_replays(text, "lsp edits", &h, &trace);
    h.keys("uu");
    assert_eq!(h.text(), text, "one `u` per batch");
    let trace = h.trace().to_text();
    assert_replays(text, "lsp edits, undone", &h, &trace);
}

/// A clipboard read is recorded as the value read (RFC-0020 R6), once per change: replay has no clipboard,
/// and puts what the live session pasted.
#[test]
fn clipboard_reads_replay_from_the_trace() {
    let clip = ruse_core::MemClipboard::new();
    clip.preload("ext\n");
    let text = "a\nb\n";
    let mut h = Headless::vim(text).with_clipboard(clip.clone());
    h.keys("\"+p\"+P");
    clip.preload("two");
    h.keys("G\"+p:put +<CR>");
    let trace = h.trace().to_text();
    assert_eq!(
        trace.matches("ex_register 43 l 6578740a").count(),
        1,
        "an unchanged clipboard is not recorded again\n{trace}"
    );
    assert!(trace.contains("ex_register 43 c 74776f"), "{trace}");
    assert_replays(text, "clipboard", &h, &trace);
    // A yank to the clipboard is a recorded command; reading back what it wrote records nothing.
    let mut h = Headless::vim(text).with_clipboard(ruse_core::MemClipboard::new());
    h.keys("\"+yyj\"+p");
    let trace = h.trace().to_text();
    assert!(!trace.contains("ex_register 43"), "{trace}");
    assert_replays(text, "clipboard yank", &h, &trace);
}

/// The registers filled before a trace starts are written at its start (RFC-0020 R6), after the options:
/// the unnamed register, `"0`-`"9`, `"a`-`"z` and `"-`, so a put of one replays the same text.
#[test]
fn registers_filled_before_the_trace_are_in_its_start() {
    let mut h = Headless::vim("one\ntwo\nthree four\n");
    h.keys("yyjdd\"ayyGdw");
    h.restart_trace();
    let text = h.text();
    h.keys("p\"0p\"1p\"ap\"-p");
    let trace = h.trace().to_text();
    let start: Vec<&str> = trace
        .lines()
        .skip(1)
        .skip_while(|l| l.starts_with("ex_set"))
        .take_while(|l| l.starts_with("ex_register"))
        .collect();
    assert_eq!(
        start,
        [
            "ex_register - c 746872656520",
            "ex_register 48 l 6f6e650a",
            "ex_register 49 l 74776f0a",
            "ex_register 45 c 746872656520",
            "ex_register 97 l 746872656520666f75720a",
        ],
        "{trace}"
    );
    assert_replays(&text, "start registers", &h, &trace);
}

/// Keys and Ex commands a random session is made of. Every Ex edit the trace records is here, with undo,
/// redo, dot-repeat and a macro so the undo grouping is exercised.
const PIECES: &[&str] = &[
    "x",
    "dd",
    "p",
    "u",
    "<Esc><C-r>",
    // The registers the session writes (RFC-0020 R6): the read-only ones, a macro's keys, the clipboard.
    "i<C-r>:<C-r>/<C-r>.<Esc>",
    "\"/p",
    "\".P",
    "qwxq\"wp",
    "\"+p",
    "\"+yy",
    ":put +<CR>",
    // `:keepjumps` around a jump an Ex edit makes.
    ":keepjumps 1d<CR>''",
    ":keepjumps %s/a/B/<CR>``",
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
    // Session-side cursor moves (#1134).
    "<C-l>a<CR>",
    "2z<CR>",
    "z.",
    "zfj",
    "zo",
    "zj",
    "zk",
    "[z",
    "]z",
];

/// Emacs-profile keys for the Emacs replay property (#1134): motions, edits and kills with point between
/// characters.
const EMACS_PIECES: &[&str] = &[
    "x",
    "ab",
    "<C-f>",
    "<C-b>",
    "<C-n>",
    "<C-p>",
    "<C-a>",
    "<C-e>",
    "<C-d>",
    "<C-k>",
    "<C-y>",
    "<M-f>",
    "<M-b>",
    "<M-d>",
    "<BS>",
    "<CR>",
    "<M-<>",
    "<M->>",
    "<C-u>3x",
    "<C-Space><C-f><C-w>",
];

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Any session of keys and Ex edits, replayed from its trace, ends in the live document and cursor.
    /// Some steps are not keys: `@lsp` applies a language server's edit list, `@clip` changes what
    /// another application put on the clipboard.
    #[test]
    fn random_sessions_with_ex_edits_replay_to_the_same_state(
        text in "[abc \n]{0,24}",
        picks in proptest::collection::vec(any::<proptest::sample::Index>(), 1..12),
        clips in proptest::collection::vec("[ab\n]{0,4}", 1..4),
    ) {
        let clip = ruse_core::MemClipboard::new();
        let mut h = Headless::vim(&text).with_clipboard(clip.clone());
        let mut clips = clips.iter().cycle();
        let mut keys = String::new();
        let n = PIECES.len() + 2;
        for i in &picks {
            let piece = match i.index(n) {
                i if i < PIECES.len() => {
                    h.keys(PIECES[i]);
                    PIECES[i]
                }
                i if i == PIECES.len() => {
                    // Reformat the first line, and insert at the end: disjoint, and in range.
                    let len = h.text().len();
                    let eol = h.text().find('\n').unwrap_or(len);
                    h.lsp_edits(&[(0, eol.min(1), "L"), (len, len, "\n")]);
                    "@lsp"
                }
                _ => {
                    clip.preload(clips.next().expect("cycle"));
                    "@clip"
                }
            };
            keys.push_str(piece);
        }
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
        prop_assert_eq!(st.cursor().0, h.cursor(), "keys {:?}\n{}", keys, trace.to_text());
    }

    /// Any Emacs-profile session, replayed from its trace, ends in the live document and point.
    #[test]
    fn random_emacs_sessions_replay_to_the_same_state(
        text in "[abc \n]{0,24}",
        picks in proptest::collection::vec(any::<proptest::sample::Index>(), 1..12),
    ) {
        let keys: String = picks
            .iter()
            .map(|i| EMACS_PIECES[i.index(EMACS_PIECES.len())])
            .collect();
        let mut h = Headless::emacs(&text);
        h.keys(&keys);
        let trace = h.trace();
        let st = trace.replay(text.as_bytes()).expect("replay");
        prop_assert_eq!(
            String::from_utf8_lossy(st.bytes()),
            h.text(),
            "keys {:?}\n{}",
            keys,
            trace.to_text()
        );
        prop_assert_eq!(st.cursor().0, h.cursor(), "keys {:?}\n{}", keys, trace.to_text());
    }
}
