//! `:trace save` writes a trace only when it replays to the document it follows (#690, F-022).
//!
//! Before `:trace save` writes, it replays the trace: a session the trace cannot follow (`:e!` re-read the
//! file, commands ran in another buffer) is refused with the reason, and no file is written. A window
//! switch on the start buffer records that window's cursor, so it replays. The property: a random session
//! saves a trace that replays to the start buffer's text, and one that stays in the start buffer is never
//! refused.

use proptest::prelude::*;
use ruse_core::{DocumentId, Trace};
use ruse_tui::headless::Headless;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A unique scratch path per call (parallel-test safe).
fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "ruse-trace-check-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The start buffer's text (the trace follows it, whichever buffer is focused now).
fn start_text(h: &Headless) -> Vec<u8> {
    h.workspace()
        .document(DocumentId(1))
        .map(|d| d.bytes().to_vec())
        .unwrap_or_default()
}

/// `:trace save` after the session so far: `Ok(trace text)` when saved, `Err(status)` when refused.
fn save(h: &mut Headless) -> Result<String, String> {
    let path = scratch("t.trace");
    h.keys(&format!("<Esc>:trace save {}<CR>", path.display()));
    let saved = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let status = h.status().to_string();
    if status.starts_with("trace saved") {
        Ok(saved.expect("a saved trace is on disk"))
    } else {
        assert!(saved.is_err(), "a refused trace is not written: {status}");
        Err(status)
    }
}

#[test]
fn a_window_switch_records_the_cursor_it_brings() {
    let text = "abc\ndef\nghi\n";
    for keys in [
        ":split<CR>jjx<C-w>wx",
        ":vsplit<CR>Gx<C-w>wjx<C-w>wx",
        ":split<CR>jx<C-w>c x",
    ] {
        let mut h = Headless::vim(text);
        h.keys(keys);
        let saved = save(&mut h).unwrap_or_else(|e| panic!("{keys:?}: {e}"));
        let st = Trace::from_text(&saved)
            .expect("parses")
            .replay(text.as_bytes())
            .expect("replays");
        assert_eq!(String::from_utf8_lossy(st.bytes()), h.text(), "{keys:?}");
        assert_eq!(st.cursor().0, h.cursor(), "{keys:?}: replayed cursor");
    }
}

/// #1272: a switch brings the window's wanted column too, and closing the focused first window (whose
/// tile index the remaining one takes) is a switch.
#[test]
fn a_window_switch_records_the_wanted_column_and_a_close_switches() {
    for (text, keys) in [
        // `ihi<Esc>` leaves this window wanting column 1; the other wants 0, which `G` reads.
        ("\nbb\nb ", ":split<CR>ihi<Esc><C-w>wddGx"),
        ("a", ":split<CR>ihi<Esc><C-w>wx<C-w>cp"),
        ("abc\ndef\nghi\n", "$:split<CR>0<C-w>wjx"),
    ] {
        let mut h = Headless::vim(text);
        h.keys(keys);
        let saved = save(&mut h).unwrap_or_else(|e| panic!("{keys:?}: {e}"));
        let st = Trace::from_text(&saved)
            .expect("parses")
            .replay(text.as_bytes())
            .expect("replays");
        assert_eq!(String::from_utf8_lossy(st.bytes()), h.text(), "{keys:?}");
        assert_eq!(st.cursor().0, h.cursor(), "{keys:?}: replayed cursor");
    }
}

#[test]
fn commands_in_another_buffer_refuse_the_save() {
    let mut h = Headless::vim("aaa\n");
    h.keys("x:enew<CR>ihello<Esc>:b1<CR>x");
    let err = save(&mut h).expect_err("refused");
    assert!(err.contains("commands ran in another buffer"), "{err}");
}

#[test]
fn a_visit_to_another_buffer_that_leaves_the_document_alone_saves() {
    // A visit records into the trace, but the commands do not change what the replay gives.
    let mut h = Headless::vim("aaa\n");
    h.keys(":enew<CR>:b1<CR>x");
    let saved = save(&mut h).expect("saved");
    let st = Trace::from_text(&saved).unwrap().replay(b"aaa\n").unwrap();
    assert_eq!(st.bytes(), b"aa\n");
}

#[test]
fn a_re_read_file_refuses_the_save() {
    let file = scratch("reread.txt");
    std::fs::write(&file, "aaa\n").unwrap();
    let name = file.display().to_string();
    let mut h = Headless::vim_named("aaa\n", &name);
    h.keys("x:e!<CR>x");
    let err = save(&mut h);
    let _ = std::fs::remove_file(&file);
    let err = err.expect_err("refused");
    assert!(err.contains("re-read from disk (:e!)"), "{err}");
}

#[test]
fn a_closed_start_buffer_refuses_the_save() {
    let mut h = Headless::vim("aaa\n");
    h.keys("x:enew<CR>:b1<CR>:bd!<CR>");
    assert!(
        h.workspace().document(DocumentId(1)).is_none(),
        "{}",
        h.status()
    );
    let err = save(&mut h).expect_err("refused");
    assert!(err.contains("closed"), "{err}");
}

/// Keys that stay in the start buffer: edits, Ex edits, window splits and switches.
const SAME_BUFFER: &[&str] = &[
    "x",
    "dd",
    "p",
    "u",
    "<C-r>",
    "j",
    "k",
    "w",
    "G",
    "gg",
    "ihi<Esc>",
    "oab<Esc>",
    ":s/a/b/<CR>",
    ":%s/b/c/g<CR>",
    "&",
    "g&",
    ":g/a/d<CR>",
    ":2m0<CR>",
    "/a<CR>",
    "<C-l>a<CR>",
    ":split<CR>",
    ":vsplit<CR>",
    "<C-w>w",
    "<C-w>p",
    "<C-w>c",
    ":only<CR>",
];

/// Keys that leave it.
const OTHER_BUFFER: &[&str] = &[":enew<CR>", ":b1<CR>", ":bn<CR>", "ixy<Esc>"];

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    /// A session that stays in its buffer always saves, and the saved trace replays to its text.
    #[test]
    fn a_session_in_one_buffer_saves_a_trace_that_replays(
        text in "[abc \n]{0,24}",
        picks in proptest::collection::vec(any::<proptest::sample::Index>(), 1..14),
    ) {
        let keys: String = picks.iter().map(|i| SAME_BUFFER[i.index(SAME_BUFFER.len())]).collect();
        let mut h = Headless::vim(&text);
        h.keys(&keys);
        let saved = save(&mut h);
        prop_assert!(saved.is_ok(), "keys {:?}: {:?}", keys, saved);
        let st = Trace::from_text(&saved.unwrap()).unwrap().replay(text.as_bytes()).unwrap();
        prop_assert_eq!(st.bytes(), &start_text(&h)[..], "keys {:?}", keys);
    }

    /// Any session either saves a trace that replays to the start buffer's text, or is refused.
    #[test]
    fn a_session_across_buffers_saves_a_replaying_trace_or_is_refused(
        text in "[abc \n]{0,24}",
        picks in proptest::collection::vec(any::<proptest::sample::Index>(), 1..14),
    ) {
        let n = SAME_BUFFER.len() + OTHER_BUFFER.len();
        let keys: String = picks
            .iter()
            .map(|i| match i.index(n) {
                i if i < SAME_BUFFER.len() => SAME_BUFFER[i],
                i => OTHER_BUFFER[i - SAME_BUFFER.len()],
            })
            .collect();
        let mut h = Headless::vim(&text);
        h.keys(&keys);
        match save(&mut h) {
            Ok(saved) => {
                let st = Trace::from_text(&saved).unwrap().replay(text.as_bytes()).unwrap();
                prop_assert_eq!(st.bytes(), &start_text(&h)[..], "keys {:?}", keys);
            }
            Err(status) => prop_assert!(status.starts_with("trace save refused: "), "{}", status),
        }
    }
}
